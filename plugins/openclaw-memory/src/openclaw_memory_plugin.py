"""OpenClaw entrypoint for mo-agent-runtime memory plugin.

This module is intentionally self-contained so packaged plugin artifacts can be
loaded without the full mo-agent-runtime repo on PYTHONPATH.
"""

from dataclasses import dataclass
from typing import Any, Protocol

_KNOWN_TASK_TYPES = {"code_review", "planning", "debugging", "general"}


class ContextManagerProtocol(Protocol):
    """Subset of ContextManager used by this plugin adapter."""

    def build_context(
        self,
        session_id: str,
        query: str,
        max_tokens: int = 8000,
        task_type: Any = "general",
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

    def __init__(self, context_manager: ContextManagerProtocol | None = None):
        self.context_manager = context_manager

    def set_context_manager(self, context_manager: ContextManagerProtocol) -> None:
        """Attach a context manager after construction."""
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
        context = self._require_context_manager().build_context(
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
        context = self._require_context_manager().build_context(
            session_id=session_id,
            query=query,
            max_tokens=max_tokens,
            task_type=self._parse_task_type(task_type),
        )
        return context.to_prompt()

    def _require_context_manager(self) -> ContextManagerProtocol:
        if self.context_manager is None:
            raise RuntimeError(
                "OpenClawMemoryPlugin requires a context manager. "
                "Pass it to __init__ or call set_context_manager()."
            )
        return self.context_manager

    @classmethod
    def _parse_task_type(cls, task_type: Any) -> Any:
        normalized = cls._normalize_task_type(task_type)

        # Prefer the runtime TaskType enum when mo-agent-runtime is available.
        try:
            from core.context.manager import TaskType as RuntimeTaskType
        except Exception:
            return normalized

        try:
            return RuntimeTaskType(normalized)
        except ValueError:
            return RuntimeTaskType.GENERAL

    @staticmethod
    def _normalize_task_type(task_type: Any) -> str:
        normalized = str(task_type).strip().lower()
        if normalized not in _KNOWN_TASK_TYPES:
            return "general"
        return normalized


__all__ = ["MemorySnippet", "OpenClawMemoryPlugin"]
