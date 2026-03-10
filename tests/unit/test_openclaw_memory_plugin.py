from dataclasses import dataclass

from core.context.manager import TaskType
from core.context.openclaw_memory_plugin import OpenClawMemoryPlugin


@dataclass
class FakeContext:
    selected_events: list[dict]

    def to_prompt(self) -> str:
        return "assembled prompt"


class FakeContextManager:
    def __init__(self):
        self.calls = []

    def build_context(self, session_id: str, query: str, max_tokens: int, task_type: TaskType):
        self.calls.append(
            {
                "session_id": session_id,
                "query": query,
                "max_tokens": max_tokens,
                "task_type": task_type,
            }
        )
        return FakeContext(
            selected_events=[
                {
                    "event_id": "evt-1",
                    "event_type": "user_message",
                    "content": "remember this",
                    "score": 0.9,
                }
            ]
        )


def test_retrieve_relevant_memory_maps_selected_events():
    manager = FakeContextManager()
    plugin = OpenClawMemoryPlugin(manager)

    snippets = plugin.retrieve_relevant_memory(
        session_id="s-1",
        query="what did I ask before?",
        max_tokens=2000,
        task_type="planning",
    )

    assert len(snippets) == 1
    assert snippets[0].event_id == "evt-1"
    assert snippets[0].score == 0.9
    assert manager.calls[0]["task_type"] == TaskType.PLANNING


def test_build_context_prompt_falls_back_to_general_task_type():
    manager = FakeContextManager()
    plugin = OpenClawMemoryPlugin(manager)

    prompt = plugin.build_context_prompt(
        session_id="s-1",
        query="summarize",
        task_type="unknown-task",
    )

    assert prompt == "assembled prompt"
    assert manager.calls[0]["task_type"] == TaskType.GENERAL


def test_retrieve_relevant_memory_normalizes_task_type_input():
    manager = FakeContextManager()
    plugin = OpenClawMemoryPlugin(manager)

    plugin.retrieve_relevant_memory(
        session_id="s-1",
        query="plan this",
        task_type="  PLANNING  ",
    )

    assert manager.calls[0]["task_type"] == TaskType.PLANNING
