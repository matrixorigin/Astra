import runpy
from dataclasses import dataclass
from pathlib import Path
from typing import Any

PLUGIN_MODULE = runpy.run_path(
    str(
        Path(__file__).resolve().parents[2]
        / "plugins"
        / "openclaw-memory"
        / "src"
        / "openclaw_memory_plugin.py"
    )
)
register = PLUGIN_MODULE["register"]


@dataclass
class FakeContext:
    selected_events: list[dict[str, Any]]

    def to_prompt(self) -> str:
        return "context prompt"


class FakeContextManager:
    def __init__(self):
        self.calls: list[dict[str, Any]] = []

    def build_context(self, session_id: str, query: str, max_tokens: int, task_type: Any) -> FakeContext:
        self.calls.append(
            {
                "session_id": session_id,
                "query": query,
                "max_tokens": max_tokens,
                "task_type": task_type,
            }
        )
        if query == "fallback":
            return FakeContext(selected_events=[])

        return FakeContext(
            selected_events=[
                {
                    "event_id": "evt-1",
                    "event_type": "user_message",
                    "content": "persist this detail",
                    "score": 0.91,
                }
            ]
        )


class FakeEventStore:
    def __init__(self):
        self.created: list[dict[str, Any]] = []
        self.deleted: list[str] = []
        self.updated: list[dict[str, Any]] = []

    def store_memory(
        self,
        *,
        session_id: str,
        user_id: str,
        text: str,
        category: str,
        importance: float,
        metadata: dict[str, Any] | None = None,
    ) -> str:
        memory_id = f"mem-{len(self.created) + 1}"
        self.created.append(
            {
                "memory_id": memory_id,
                "session_id": session_id,
                "user_id": user_id,
                "text": text,
                "category": category,
                "importance": importance,
                "metadata": metadata or {},
            }
        )
        return memory_id

    def delete_memory(self, *, memory_id: str) -> bool:
        self.deleted.append(memory_id)
        return memory_id != "missing"

    def update_memory(
        self,
        *,
        memory_id: str,
        text: str | None = None,
        category: str | None = None,
        importance: float | None = None,
    ) -> bool:
        self.updated.append(
            {
                "memory_id": memory_id,
                "text": text,
                "category": category,
                "importance": importance,
            }
        )
        return memory_id != "missing"

    def search_memory_ids(self, *, session_id: str, query: str, limit: int) -> list[str]:
        if query == "fallback":
            return ["mem-9"]
        return []


class FakeHostApi:
    def __init__(self):
        self.tools: dict[str, dict[str, Any]] = {}
        self.hooks: dict[str, Any] = {}

    def registerTool(self, factory: Any, *_args: Any, **_kwargs: Any) -> None:  # noqa: N802
        tool = factory()
        self.tools[tool["name"]] = tool

    def on(self, hook_name: str, callback: Any) -> None:
        self.hooks[hook_name] = callback


def _setup() -> tuple[FakeHostApi, FakeContextManager, FakeEventStore]:
    api = FakeHostApi()
    context_manager = FakeContextManager()
    event_store = FakeEventStore()
    register(api, {}, context_manager=context_manager, event_store=event_store)
    return api, context_manager, event_store


def test_register_adds_all_p1_tools_and_hooks():
    api, _, _ = _setup()

    assert set(api.tools) == {
        "memory_recall",
        "memory_store",
        "memory_forget",
        "memory_update",
    }
    assert set(api.hooks) == {"before_prompt_build", "before_agent_start", "agent_end"}


def test_memory_recall_tool_returns_expected_payload():
    api, context_manager, _ = _setup()
    response = api.tools["memory_recall"]["execute"]({"session_id": "s-1", "query": "what matters", "limit": 1})

    assert response["details"]["count"] == 1
    assert response["details"]["memories"][0]["event_id"] == "evt-1"
    assert response["details"]["query"] == "what matters"
    assert context_manager.calls[0]["session_id"] == "s-1"
    assert "Found 1 memories" in response["content"][0]["text"]


def test_memory_store_forget_and_update_tools_return_expected_results():
    api, _, event_store = _setup()

    store_response = api.tools["memory_store"]["execute"](
        {"session_id": "s-1", "user_id": "u-1", "text": "remember this"}
    )
    assert store_response["details"]["action"] == "created"
    assert store_response["details"]["memory_id"] == "mem-1"
    assert event_store.created[0]["metadata"]["memory_source"] == "tool.memory_store"

    forget_response = api.tools["memory_forget"]["execute"]({"memory_id": "mem-1"})
    assert forget_response["details"]["action"] == "deleted"
    assert forget_response["details"]["memory_ids"] == ["mem-1"]

    update_response = api.tools["memory_update"]["execute"]({"memory_id": "mem-1", "text": "new text"})
    assert update_response["details"]["action"] == "updated"
    assert update_response["details"]["updated_fields"] == ["text"]


def test_memory_forget_uses_search_fallback_when_recall_has_no_candidates():
    api, _, _ = _setup()

    response = api.tools["memory_forget"]["execute"](
        {"session_id": "s-1", "query": "fallback", "limit": 1}
    )

    assert response["details"]["action"] == "deleted"
    assert response["details"]["memory_ids"] == ["mem-9"]


def test_memory_update_tool_validates_update_fields():
    api, _, _ = _setup()

    response = api.tools["memory_update"]["execute"]({"memory_id": "mem-1"})

    assert response["details"]["error"] == "memory_update_failed"
    assert "Provide at least one field to update" in response["details"]["message"]
