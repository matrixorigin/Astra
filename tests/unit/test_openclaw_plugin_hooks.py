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
    def __init__(self, selected_events: list[dict[str, Any]]):
        self.selected_events = selected_events
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
        return FakeContext(selected_events=self.selected_events)


class FakeEventStore:
    def __init__(self):
        self.created: list[dict[str, Any]] = []

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
        return memory_id != "missing"

    def update_memory(
        self,
        *,
        memory_id: str,
        text: str | None = None,
        category: str | None = None,
        importance: float | None = None,
    ) -> bool:
        return memory_id != "missing"

    def search_memory_ids(self, *, session_id: str, query: str, limit: int) -> list[str]:
        return []


class FakeHostApi:
    def __init__(self):
        self.tools: dict[str, Any] = {}
        self.hooks: dict[str, Any] = {}

    def registerTool(self, factory: Any, *_args: Any, **_kwargs: Any) -> None:  # noqa: N802
        tool = factory()
        self.tools[tool["name"]] = tool

    def on(self, hook_name: str, callback: Any) -> None:
        self.hooks[hook_name] = callback


def _build_plugin(
    *,
    auto_recall: bool = False,
    auto_capture: bool = True,
    capture_assistant: bool = False,
) -> tuple[Any, FakeHostApi, FakeContextManager, FakeEventStore]:
    api = FakeHostApi()
    context_manager = FakeContextManager(
        selected_events=[
            {
                "event_id": "evt-1",
                "event_type": "user_message",
                "content": "Remember the DB DSN override from last run.",
                "score": 0.95,
            },
            {
                "event_id": "evt-2",
                "event_type": "system_message",
                "content": "Use short hooks output.",
                "score": 0.82,
            },
        ]
    )
    event_store = FakeEventStore()
    plugin = register(
        api,
        {
            "auto_recall": auto_recall,
            "auto_capture": auto_capture,
            "capture_assistant": capture_assistant,
            "recall_limit": 2,
            "capture_max_items": 2,
        },
        context_manager=context_manager,
        event_store=event_store,
    )
    return plugin, api, context_manager, event_store


def test_before_prompt_build_hook_returns_prepend_context_when_enabled():
    _, api, context_manager, _ = _build_plugin(auto_recall=True)

    response = api.hooks["before_prompt_build"](
        {"session_id": "s-1", "prompt": "How should I configure cache?"}
    )

    assert response is not None
    assert "prependContext" in response
    assert "<relevant-memories>" in response["prependContext"]
    assert "[user_message] Remember the DB DSN override from last run." in response["prependContext"]
    assert context_manager.calls[0]["session_id"] == "s-1"
    assert context_manager.calls[0]["query"] == "How should I configure cache?"


def test_before_agent_start_hook_keeps_legacy_prepend_context_alias():
    _, api, _, _ = _build_plugin(auto_recall=True)

    response = api.hooks["before_agent_start"](
        {"session_id": "s-1", "prompt": "How should I configure cache?"}
    )

    assert response is not None
    assert "<relevant-memories>" in response["prependContext"]


def test_before_prompt_build_hook_returns_none_when_disabled():
    _, api, context_manager, _ = _build_plugin(auto_recall=False)

    response = api.hooks["before_prompt_build"]({"session_id": "s-1", "prompt": "ignored"})

    assert response is None
    assert context_manager.calls == []


def test_agent_end_hook_stores_unique_user_messages_when_enabled():
    _, api, _, event_store = _build_plugin(auto_capture=True, capture_assistant=False)

    response = api.hooks["agent_end"](
        {
            "session_id": "s-2",
            "user_id": "u-2",
            "success": True,
            "messages": [
                {"role": "user", "content": "  use poetry run ruff  "},
                {"role": "assistant", "content": "ack"},
                {"role": "user", "content": "use poetry run ruff"},
            ],
        }
    )

    assert response == {"captured": 1, "memory_ids": ["mem-1"]}
    assert len(event_store.created) == 1
    assert event_store.created[0]["session_id"] == "s-2"
    assert event_store.created[0]["user_id"] == "u-2"
    assert event_store.created[0]["text"] == "use poetry run ruff"
    assert event_store.created[0]["metadata"]["memory_source"] == "hook.agent_end"


def test_agent_end_hook_returns_none_when_disabled():
    _, api, _, event_store = _build_plugin(auto_capture=False)

    response = api.hooks["agent_end"](
        {
            "session_id": "s-2",
            "success": True,
            "messages": [{"role": "user", "content": "capture me"}],
        }
    )

    assert response is None
    assert event_store.created == []
