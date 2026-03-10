"""OpenClaw plugin adapter for the platform memory/context layer."""

from dataclasses import dataclass
from typing import Protocol

from core.context.manager import TaskType


class ContextManagerProtocol(Protocol):
    """Subset of ContextManager used by this plugin adapter."""

    def build_context(
        self,
        session_id: str,
        query: str,
        max_tokens: int = 8000,
        task_type: TaskType = TaskType.GENERAL,
    ): ...


@dataclass
class MemorySnippet:
    """Serializable memory snippet for plugin consumers."""

    event_id: str
    event_type: str
    content: str
    score: float


class OpenClawMemoryPlugin:
    """Expose ContextManager memory selection in an OpenClaw-friendly shape."""

    def __init__(self, context_manager: ContextManagerProtocol):
        self.context_manager = context_manager

    def retrieve_relevant_memory(
        self,
        *,
        session_id: str,
        query: str,
        max_tokens: int = 4000,
        task_type: str = "general",
    ) -> list[MemorySnippet]:
        """Return selected memory entries from the context layer."""
        context = self.context_manager.build_context(
            session_id=session_id,
            query=query,
            max_tokens=max_tokens,
            task_type=self._parse_task_type(task_type),
        )

        return [
            MemorySnippet(
                event_id=event["event_id"],
                event_type=event["event_type"],
                content=event["content"],
                score=event["score"],
            )
            for event in context.selected_events
        ]

    def build_context_prompt(
        self,
        *,
        session_id: str,
        query: str,
        max_tokens: int = 4000,
        task_type: str = "general",
    ) -> str:
        """Build prompt text from selected memory/context."""
        context = self.context_manager.build_context(
            session_id=session_id,
            query=query,
            max_tokens=max_tokens,
            task_type=self._parse_task_type(task_type),
        )
        return context.to_prompt()

    @staticmethod
    def _parse_task_type(task_type: str) -> TaskType:
        normalized = str(task_type).strip().lower()
        try:
            return TaskType(normalized)
        except (TypeError, ValueError):
            return TaskType.GENERAL
