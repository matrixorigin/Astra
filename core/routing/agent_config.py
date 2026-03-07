"""Agent configurations for different specializations."""

from typing import Dict, List, Optional

from pydantic import BaseModel

from core.routing.query_router import AgentType


class AgentConfig(BaseModel):
    """Configuration for a specialized agent."""
    agent_type: AgentType
    system_prompt: str
    preferred_tools: List[str]
    max_context_tokens: int = 8000
    temperature: float = 0.7


class AgentConfigManager:
    """Manages configurations for different agent types."""
    
    def __init__(self):
        self.configs = {
            AgentType.CODE: AgentConfig(
                agent_type=AgentType.CODE,
                system_prompt="""You are a specialized code assistant. You excel at:
- Writing, reviewing, and debugging code
- Explaining programming concepts
- Suggesting best practices and optimizations
- Working with version control and development workflows

Focus on providing accurate, efficient, and well-documented code solutions.""",
                preferred_tools=[
                    "read_file", "write_file", "list_files", "search_files",
                    "run_command", "git_status", "git_diff", "create_file"
                ],
                temperature=0.3
            ),
            
            AgentType.PLANNING: AgentConfig(
                agent_type=AgentType.PLANNING,
                system_prompt="""You are a specialized planning assistant. You excel at:
- Breaking down complex projects into manageable tasks
- Creating structured plans and roadmaps
- Analyzing requirements and scope
- Suggesting methodologies and approaches

Focus on creating clear, actionable plans with realistic timelines and dependencies.""",
                preferred_tools=[
                    "create_file", "write_file", "search_files",
                    "web_search", "analyze_requirements"
                ],
                temperature=0.5
            ),
            
            AgentType.DEBUGGING: AgentConfig(
                agent_type=AgentType.DEBUGGING,
                system_prompt="""You are a specialized debugging assistant. You excel at:
- Identifying and fixing bugs and errors
- Analyzing error messages and stack traces
- Troubleshooting system issues
- Suggesting diagnostic approaches

Focus on systematic problem-solving and root cause analysis.""",
                preferred_tools=[
                    "read_file", "search_files", "run_command", "check_logs",
                    "analyze_error", "test_code", "git_log"
                ],
                temperature=0.2
            ),
            
            AgentType.GENERAL: AgentConfig(
                agent_type=AgentType.GENERAL,
                system_prompt="""You are a helpful general-purpose assistant. You can help with:
- Answering questions and providing information
- General problem-solving and analysis
- Writing and editing tasks
- Research and information gathering

Adapt your approach based on the specific needs of each request.""",
                preferred_tools=[
                    "web_search", "read_file", "write_file", "create_file",
                    "run_command", "search_files"
                ],
                temperature=0.7
            )
        }
    
    def get_config(self, agent_type: AgentType) -> AgentConfig:
        """Get configuration for specified agent type."""
        return self.configs.get(agent_type, self.configs[AgentType.GENERAL])
    
    def get_system_prompt(self, agent_type: AgentType) -> str:
        """Get system prompt for specified agent type."""
        return self.get_config(agent_type).system_prompt
    
    def get_preferred_tools(self, agent_type: AgentType) -> List[str]:
        """Get preferred tools for specified agent type."""
        return self.get_config(agent_type).preferred_tools