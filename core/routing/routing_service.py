"""Main routing service that coordinates query analysis and agent selection."""

from typing import Dict, Any, Optional

from pydantic import BaseModel

from core.routing.query_router import QueryRouter, RoutingResult, AgentType
from core.routing.agent_config import AgentConfigManager, AgentConfig


class RoutingDecision(BaseModel):
    """Complete routing decision with agent config."""

    routing_result: RoutingResult
    agent_config: AgentConfig
    context_modifications: Dict[str, Any]


class RoutingService:
    """Main service for routing queries to appropriate agents."""

    def __init__(self):
        self.query_router = QueryRouter()
        self.config_manager = AgentConfigManager()

    def route_query(self, query: str, context: Optional[Dict[str, Any]] = None) -> RoutingDecision:
        """Route a query and return complete routing decision."""
        # Analyze query to determine agent type
        routing_result = self.query_router.route(query)

        # Get agent configuration
        agent_config = self.config_manager.get_config(routing_result.agent_type)

        # Prepare context modifications
        context_modifications = self._prepare_context_modifications(
            routing_result, agent_config, context or {}
        )

        return RoutingDecision(
            routing_result=routing_result,
            agent_config=agent_config,
            context_modifications=context_modifications,
        )

    def _prepare_context_modifications(
        self,
        routing_result: RoutingResult,
        agent_config: AgentConfig,
        original_context: Dict[str, Any],
    ) -> Dict[str, Any]:
        """Prepare context modifications based on routing decision."""
        modifications = {
            "agent_type": routing_result.agent_type.value,
            "routing_confidence": routing_result.confidence,
            "matched_patterns": routing_result.matched_patterns,
            "system_prompt": agent_config.system_prompt,
            "preferred_tools": agent_config.preferred_tools,
            "temperature": agent_config.temperature,
            "max_context_tokens": agent_config.max_context_tokens,
        }

        # Merge with original context, giving priority to routing decisions
        return {**original_context, **modifications}
