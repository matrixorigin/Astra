"""JSON bridge for the TypeScript OpenClaw host package."""

from __future__ import annotations

import json
import os
import runpy
import sys
from dataclasses import asdict
from pathlib import Path
from typing import Any

SRC_DIR = Path(__file__).resolve().parent
PACKAGE_DIR = SRC_DIR.parent
PLUGIN_MODULE = SRC_DIR / "openclaw_memory_plugin.py"


class _NoopApi:
    def registerTool(self, *_args: Any, **_kwargs: Any) -> None:
        return None

    def on(self, *_args: Any, **_kwargs: Any) -> None:
        return None


def _maybe_add_runtime_root(config: dict[str, Any]) -> None:
    candidates: list[Path] = []

    configured_root = config.get("runtimeRoot") or config.get("runtime_root")
    env_root = os.environ.get("MO_AGENT_RUNTIME_ROOT")
    for raw in (configured_root, env_root):
        if isinstance(raw, str) and raw.strip():
            candidates.append(Path(raw).expanduser().resolve())

    repo_root = PACKAGE_DIR.parents[1]
    if (repo_root / "core" / "context" / "manager.py").exists():
        candidates.append(repo_root)

    for candidate in candidates:
        candidate_text = str(candidate)
        if candidate.exists() and candidate_text not in sys.path:
            sys.path.insert(0, candidate_text)


def _load_plugin(config: dict[str, Any]):
    _maybe_add_runtime_root(config)
    module = runpy.run_path(str(PLUGIN_MODULE))
    register = module["register"]
    return register(_NoopApi(), config)


def _dispatch(plugin: Any, action: str, params: dict[str, Any]) -> Any:
    if action == "memory_recall":
        snippets = plugin.retrieve_relevant_memory(
            session_id=str(params.get("session_id", "")),
            query=str(params.get("query", "")),
            max_tokens=int(params.get("max_tokens", 4000)),
            task_type=str(params.get("task_type", "general")),
        )
        return [asdict(snippet) for snippet in snippets]

    if action == "memory_store":
        return plugin.memory_store(
            session_id=str(params.get("session_id", "")),
            user_id=str(params.get("user_id", plugin.config["default_user_id"])),
            text=str(params.get("text", "")),
            category=str(params.get("category", "other")),
            importance=float(params.get("importance", 0.7)),
            source=str(params.get("source", "tool.memory_store")),
        )

    if action == "memory_forget":
        return plugin.memory_forget(memory_id=str(params.get("memory_id", "")))

    if action == "memory_update":
        kwargs = {
            "memory_id": str(params.get("memory_id", "")),
            "text": params.get("text"),
            "category": params.get("category"),
            "importance": params.get("importance"),
        }
        return plugin.memory_update(**kwargs)

    if action == "search_memory_ids":
        store = plugin._require_event_store()
        return store.search_memory_ids(
            session_id=str(params.get("session_id", "")),
            query=str(params.get("query", "")),
            limit=int(params.get("limit", 1)),
        )

    raise ValueError(f"Unsupported action: {action}")


def main() -> int:
    try:
        request = json.loads(sys.stdin.read() or "{}")
        config = request.get("config") if isinstance(request.get("config"), dict) else {}
        params = request.get("params") if isinstance(request.get("params"), dict) else {}
        action = str(request.get("action", "")).strip()
        if not action:
            raise ValueError("Missing bridge action")

        plugin = _load_plugin(config)
        result = _dispatch(plugin, action, params)
        print(json.dumps({"ok": True, "result": result}))
    except Exception as exc:  # pragma: no cover - subprocess boundary
        print(
            json.dumps(
                {
                    "ok": False,
                    "error": {
                        "type": exc.__class__.__name__,
                        "message": str(exc),
                    },
                }
            )
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
