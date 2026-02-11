"""Agent Registry for managing agent profiles."""

from typing import Dict, List, Optional
from pydantic import BaseModel


class AgentProfile(BaseModel):
    """Profile for an agent."""

    agent_id: str  # e.g. "code_reviewer", "security_auditor"
    system_prompt: str  # Role-specific instructions
    skill_filter: List[str] | None = None  # Limit available skills
    model: str | None = None  # Optional model override


class AgentRegistry:
    """Registry for managing agent profiles."""

    def __init__(self):
        self._agents: Dict[str, AgentProfile] = {}

    def register(self, profile: AgentProfile) -> None:
        """Register a new agent profile."""
        self._agents[profile.agent_id] = profile

    def get(self, agent_id: str) -> AgentProfile | None:
        """Get an agent profile by ID."""
        return self._agents.get(agent_id)

    def list_agents(self) -> List[AgentProfile]:
        """List all registered agents."""
        return list(self._agents.values())

    def unregister(self, agent_id: str) -> bool:
        """Remove an agent profile. Returns True if removed."""
        if agent_id in self._agents:
            del self._agents[agent_id]
            return True
        return False
