"""Introspection tool — lets the agent query its own runtime state.

Design doc: docs/design/agent-introspection.md

Handles the 20% of introspection queries that need runtime data
(the other 80% is answered from the Self-Model section in the system prompt).

Trigger examples in tool description guide the LLM on when to call this
vs answering from the system prompt directly.
"""

import json
import logging
from typing import Any

from cli.tools.base import EdgeTool, SideEffect, resolve_side_effect


class GetAgentInfoTool(EdgeTool):
    """Introspection tool for runtime agent state."""

    def __init__(
        self,
        tool_router: Any = None,
        session_info: dict[str, Any] | None = None,
        api_client: Any = None,
    ):
        self._router = tool_router
        # Mutable dict — caller can update fields (e.g. turn count) between calls.
        self._session = session_info or {}
        # Optional cloud client for memory dimension enrichment
        self._api_client = api_client

    @property
    def name(self) -> str:
        return "get_agent_info"

    @property
    def description(self) -> str:
        return (
            "Query agent runtime state: token usage, context breakdown, "
            "session info, available tools, memory stats. "
            "Use when user asks about context size, token budget, what model, or agent capabilities."
        )

    @property
    def parameters(self) -> dict[str, Any]:
        return {
            "type": "object",
            "properties": {
                "dimension": {
                    "type": "string",
                    "enum": ["capability", "state", "memory", "identity", "context_snapshot", "context_trend", "retrieval_quality", "all"],
                    "description": (
                        "Which dimension to query. "
                        "'context_snapshot': token usage, zone balance, relevance for a specific turn. "
                        "'context_trend': token growth across recent turns. "
                        "'retrieval_quality': memory retrieval effectiveness. "
                        "'all' returns everything except raw context content."
                    ),
                },
                "turn_index": {
                    "type": "integer",
                    "description": "For context_snapshot: which turn to inspect (1-based). Omit for latest turn.",
                },
            },
            "required": ["dimension"],
        }

    @property
    def side_effect(self) -> SideEffect:
        return SideEffect.READ

    _VALID_DIMENSIONS = {"capability", "state", "memory", "identity", "context_snapshot", "context_trend", "retrieval_quality", "all"}

    async def execute(self, dimension: str = "all", **kwargs: Any) -> str:
        if dimension not in self._VALID_DIMENSIONS:
            return json.dumps({"error": f"Invalid dimension '{dimension}'. Valid: {sorted(self._VALID_DIMENSIONS)}"})

        info: dict[str, Any] = {}

        if dimension in ("capability", "all"):
            tools = []
            if self._router:
                for t in self._router.list_tools():
                    tools.append({"name": t.name, "side_effect": resolve_side_effect(t).value})
            info["capability"] = {
                "tools": tools,
                "tool_count": len(tools),
            }
            # Enrich with cloud skills (installed + catalog) if API client available.
            if self._api_client:
                try:
                    skills_data = await self._api_client.get_introspection_skills()
                    info["capability"]["installed_skills"] = skills_data.get("installed", [])
                    info["capability"]["cloud_skills"] = skills_data.get("cloud", [])
                except Exception as exc:
                    logging.getLogger(__name__).debug(
                        "Cloud skills enrichment unavailable: %s", exc
                    )

        if dimension in ("state", "all"):
            info["state"] = {
                "session_id": self._session.get("session_id"),
                "turn": self._session.get("turn", 0),
                "agent_id": self._session.get("agent_id"),
                "model": self._session.get("model"),
                "prompt_tokens": self._session.get("prompt_tokens"),
                "completion_tokens": self._session.get("completion_tokens"),
            }

        if dimension in ("memory", "all"):
            info["memory"] = {
                "has_project_rules": self._session.get("has_project_rules", False),
                "has_edge_profile": self._session.get("has_edge_profile", False),
            }
            # Enrich with cloud data if available.
            # execute() is async, so we can await directly — no run_until_complete needed.
            # Graceful degrade: network failure / missing session → keep local data only.
            if self._api_client and self._session.get("session_id"):
                try:
                    cloud_memory = await self._api_client.get_introspection_memory(
                        self._session["session_id"]
                    )
                    info["memory"].update(cloud_memory)
                except Exception as exc:
                    logging.getLogger(__name__).debug(
                        "Cloud memory enrichment unavailable: %s", exc
                    )

        if dimension in ("identity", "all"):
            info["identity"] = {
                "agent_id": self._session.get("agent_id"),
                "agent_type": self._session.get("agent_type", "default"),
            }

        session_id = self._session.get("session_id")

        if dimension in ("context_snapshot",):
            turn_index = kwargs.get("turn_index")
            if self._api_client and session_id:
                try:
                    info["context_snapshot"] = await self._api_client.get_introspection_context_snapshot(
                        session_id, turn_index=turn_index, detail=True
                    )
                except Exception as exc:
                    info["context_snapshot"] = {"error": str(exc)}
            else:
                info["context_snapshot"] = {"error": "no session or api_client"}

        if dimension in ("context_trend",):
            if self._api_client and session_id:
                try:
                    info["context_trend"] = await self._api_client.get_introspection_context_trend(session_id)
                except Exception as exc:
                    info["context_trend"] = {"error": str(exc)}
            else:
                info["context_trend"] = {"error": "no session or api_client"}

        if dimension in ("retrieval_quality",):
            if self._api_client and session_id:
                try:
                    info["retrieval_quality"] = await self._api_client.get_introspection_retrieval_quality(session_id)
                except Exception as exc:
                    info["retrieval_quality"] = {"error": str(exc)}
            else:
                info["retrieval_quality"] = {"error": "no session or api_client"}

        return json.dumps(info, indent=2)
