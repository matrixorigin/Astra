"""Agent Registry for managing agent profiles."""

from typing import Literal

from pydantic import BaseModel, model_validator


class AgentProfile(BaseModel):
    """Profile for an agent.
    
    Defines agent capabilities, permissions, and behavior according to
    the multi-agent architecture in agents-and-orchestration.md.
    """

    agent_id: str  # e.g. "code_reviewer", "security_auditor"
    system_prompt: str  # Role-specific instructions
    skill_filter: list[str] | None = None  # Limit available skills
    model: str | None = None  # Optional model override
    can_delegate: bool = False  # Can delegate to other agents
    delegate_to: list[str] = []  # Whitelist of agents this agent can delegate to
    tier: Literal["user", "system", "orchestrator"] = "user"  # Agent tier
    triggers: list[str] = []  # Auto-trigger events (system agents only)

    @model_validator(mode="after")
    def validate_agent_profile(self) -> "AgentProfile":
        """Validate agent profile constraints."""
        # Validate triggers are only set for system agents
        if self.triggers and self.tier != "system":
            raise ValueError("Only system agents can have triggers")
        
        # Validate delegation permissions
        if self.can_delegate and self.tier not in ["orchestrator", "system"]:
            raise ValueError("Only orchestrator and system agents can delegate")
        
        return self


class AgentRegistry:
    """Registry for managing agent profiles."""

    def __init__(self):
        self._agents: dict[str, AgentProfile] = {}

    def register(self, profile: AgentProfile) -> None:
        """Register a new agent profile.
        
        Validates delegate_to references exist.
        """
        # Validate delegate_to references
        for target_id in profile.delegate_to:
            if target_id not in self._agents:
                raise ValueError(f"Delegate target '{target_id}' not found in registry")
        
        self._agents[profile.agent_id] = profile

    def _ensure_initialized(self) -> None:
        """Ensure registry is initialized."""
        pass

    def get(self, agent_id: str) -> AgentProfile | None:
        """Get an agent profile by ID."""
        return self._agents.get(agent_id)

    def list_agents(self) -> list[AgentProfile]:
        """List all registered agents."""
        return list(self._agents.values())

    def unregister(self, agent_id: str) -> bool:
        """Remove an agent profile. Returns True if removed."""
        if agent_id in self._agents:
            del self._agents[agent_id]
            return True
        return False
    
    def can_delegate(self, from_agent: str, to_agent: str) -> bool:
        """Check if from_agent can delegate to to_agent."""
        profile = self.get(from_agent)
        if not profile or not profile.can_delegate:
            return False
        # Empty delegate_to means can delegate to anyone
        if not profile.delegate_to:
            return True
        return to_agent in profile.delegate_to
