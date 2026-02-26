"""Global Context Budget Manager.

Manages context budget allocation across all sources:
- System prompt
- Conversation history
- Tool outputs
- Memory (L0/L1)
- Code context
- Documentation
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum


class ConversationStage(str, Enum):
    """Conversation stage for dynamic budget allocation."""
    QUERY = "query"           # Simple question
    ANALYSIS = "analysis"     # Deep analysis/debugging
    GENERATION = "generation" # Code generation
    PLANNING = "planning"     # Multi-step planning


@dataclass
class BudgetAllocation:
    """Budget allocation in tokens for each context source."""
    system_prompt: int = 0
    history: int = 0
    tool_output: int = 0
    memory_l0: int = 0
    memory_l1: int = 0
    code_context: int = 0
    documentation: int = 0
    
    @property
    def total(self) -> int:
        return (
            self.system_prompt + self.history + self.tool_output +
            self.memory_l0 + self.memory_l1 + self.code_context + self.documentation
        )


# Default budget ratios by stage (must sum to 1.0)
STAGE_BUDGETS: dict[ConversationStage, dict[str, float]] = {
    ConversationStage.QUERY: {
        "system_prompt": 0.10,
        "history": 0.25,
        "tool_output": 0.25,
        "memory_l0": 0.05,
        "memory_l1": 0.15,
        "code_context": 0.10,
        "documentation": 0.10,
    },
    ConversationStage.ANALYSIS: {
        "system_prompt": 0.08,
        "history": 0.15,
        "tool_output": 0.35,  # More for tool outputs
        "memory_l0": 0.05,
        "memory_l1": 0.12,
        "code_context": 0.15,
        "documentation": 0.10,
    },
    ConversationStage.GENERATION: {
        "system_prompt": 0.10,
        "history": 0.20,
        "tool_output": 0.20,
        "memory_l0": 0.05,
        "memory_l1": 0.10,
        "code_context": 0.25,  # More for code context
        "documentation": 0.10,
    },
    ConversationStage.PLANNING: {
        "system_prompt": 0.10,
        "history": 0.30,  # More history for planning
        "tool_output": 0.20,
        "memory_l0": 0.05,
        "memory_l1": 0.15,
        "code_context": 0.10,
        "documentation": 0.10,
    },
}


class ContextBudgetManager:
    """Manages global context budget allocation."""
    
    def __init__(self, max_context_tokens: int = 128000, reserve_for_output: int = 4000):
        """Initialize budget manager.
        
        Args:
            max_context_tokens: Maximum context window size
            reserve_for_output: Tokens to reserve for model output
        """
        self.max_context_tokens = max_context_tokens
        self.reserve_for_output = reserve_for_output
        self._used: dict[str, int] = {}
    
    @property
    def available_tokens(self) -> int:
        """Total available tokens for context."""
        return self.max_context_tokens - self.reserve_for_output
    
    @property
    def used_tokens(self) -> int:
        """Total tokens used so far."""
        return sum(self._used.values())
    
    @property
    def remaining_tokens(self) -> int:
        """Remaining tokens available."""
        return self.available_tokens - self.used_tokens
    
    def allocate(self, stage: ConversationStage | str = ConversationStage.QUERY) -> BudgetAllocation:
        """Allocate budget based on conversation stage.
        
        Args:
            stage: Current conversation stage
        
        Returns:
            BudgetAllocation with token limits for each source
        """
        if isinstance(stage, str):
            stage = ConversationStage(stage)
        
        ratios = STAGE_BUDGETS.get(stage, STAGE_BUDGETS[ConversationStage.QUERY])
        available = self.available_tokens
        
        return BudgetAllocation(
            system_prompt=int(available * ratios["system_prompt"]),
            history=int(available * ratios["history"]),
            tool_output=int(available * ratios["tool_output"]),
            memory_l0=int(available * ratios["memory_l0"]),
            memory_l1=int(available * ratios["memory_l1"]),
            code_context=int(available * ratios["code_context"]),
            documentation=int(available * ratios["documentation"]),
        )
    
    def allocate_remaining(self, stage: ConversationStage | str = ConversationStage.QUERY) -> BudgetAllocation:
        """Allocate remaining budget (after some sources already used).
        
        Args:
            stage: Current conversation stage
        
        Returns:
            BudgetAllocation with token limits for remaining sources
        """
        if isinstance(stage, str):
            stage = ConversationStage(stage)
        
        ratios = STAGE_BUDGETS.get(stage, STAGE_BUDGETS[ConversationStage.QUERY])
        remaining = self.remaining_tokens
        
        return BudgetAllocation(
            system_prompt=int(remaining * ratios["system_prompt"]),
            history=int(remaining * ratios["history"]),
            tool_output=int(remaining * ratios["tool_output"]),
            memory_l0=int(remaining * ratios["memory_l0"]),
            memory_l1=int(remaining * ratios["memory_l1"]),
            code_context=int(remaining * ratios["code_context"]),
            documentation=int(remaining * ratios["documentation"]),
        )
    
    def record_usage(self, source: str, tokens: int) -> None:
        """Record token usage for a source.
        
        Args:
            source: Source name (system_prompt, history, tool_output, etc.)
            tokens: Number of tokens used
        """
        self._used[source] = self._used.get(source, 0) + tokens
    
    def get_usage(self, source: str) -> int:
        """Get token usage for a source."""
        return self._used.get(source, 0)
    
    def reset(self) -> None:
        """Reset all usage tracking."""
        self._used.clear()
    
    def get_tool_output_budget(self, stage: ConversationStage | str = ConversationStage.QUERY) -> int:
        """Get tool output budget in bytes (for tool_output_handler).
        
        Args:
            stage: Current conversation stage
        
        Returns:
            Budget in bytes (~4 chars per token)
        """
        allocation = self.allocate_remaining(stage)
        return allocation.tool_output * 4  # ~4 chars per token


def classify_stage(query: str, tool_calls_count: int = 0) -> ConversationStage:
    """Classify conversation stage from query and context.
    
    Args:
        query: User query
        tool_calls_count: Number of tool calls so far in this turn
    
    Returns:
        Detected conversation stage
    """
    query_lower = query.lower()
    
    # Planning indicators
    if any(kw in query_lower for kw in ["plan", "step by step", "how to", "implement"]):
        return ConversationStage.PLANNING
    
    # Generation indicators
    if any(kw in query_lower for kw in ["write", "create", "generate", "build", "add"]):
        return ConversationStage.GENERATION
    
    # Analysis indicators (or many tool calls)
    if tool_calls_count > 2 or any(kw in query_lower for kw in ["debug", "analyze", "why", "investigate", "fix"]):
        return ConversationStage.ANALYSIS
    
    # Default to query
    return ConversationStage.QUERY
