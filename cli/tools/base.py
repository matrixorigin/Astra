"""Base class for edge tools."""

from abc import ABC, abstractmethod
from enum import Enum
from typing import Any


class SideEffect(str, Enum):
    """Side effect classification for permission checking."""
    READ = "read"
    WRITE = "write"
    EXECUTE = "execute"


class EdgeTool(ABC):
    """Abstract base for tools that execute on the user's machine."""

    @property
    @abstractmethod
    def name(self) -> str:
        """Tool name matching OpenAI function calling convention."""

    @property
    @abstractmethod
    def description(self) -> str:
        """Human-readable description for LLM."""

    @property
    @abstractmethod
    def parameters(self) -> dict[str, Any]:
        """JSON Schema for tool parameters."""

    @property
    @abstractmethod
    def side_effect(self) -> SideEffect:
        """Side effect level for permission checking."""

    @abstractmethod
    async def execute(self, **kwargs: Any) -> str:
        """Execute the tool and return result as string."""

    def to_openai_schema(self) -> dict[str, Any]:
        """Return OpenAI function calling tool schema."""
        return {
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            },
        }
