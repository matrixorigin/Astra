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

from cli.tools.base import EdgeTool, SideEffect


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
            "Query your own runtime state: available tools, session info, "
            "active permissions, and turn count. "
            "Use this when the user asks about your current capabilities at runtime "
            "(e.g. 'what tools do you have right now?', 'what's your session id?', "
            "'how many turns have we used?'). "
            "For general identity questions ('who are you?', 'what can you do?'), "
            "answer from your Self-Model section instead of calling this tool."
        )

    @property
    def parameters(self) -> dict[str, Any]:
        return {
            "type": "object",
            "properties": {
                "dimension": {
                    "type": "string",
                    "enum": ["capability", "state", "memory", "identity", "all"],
                    "description": "Which dimension to query. 'all' returns everything.",
                },
            },
            "required": ["dimension"],
        }

    @property
    def side_effect(self) -> SideEffect:
        return SideEffect.READ

    _VALID_DIMENSIONS = {"capability", "state", "memory", "identity", "all"}

    async def execute(self, dimension: str = "all", **kwargs: Any) -> str:
        if dimension not in self._VALID_DIMENSIONS:
            return json.dumps({"error": f"Invalid dimension '{dimension}'. Valid: {sorted(self._VALID_DIMENSIONS)}"})

        info: dict[str, Any] = {}

        if dimension in ("capability", "all"):
            tools = []
            if self._router:
                for t in self._router.list_tools():
                    tools.append({"name": t.name, "side_effect": t.side_effect.value})
            info["capability"] = {
                "tools": tools,
                "tool_count": len(tools),
            }

        if dimension in ("state", "all"):
            info["state"] = {
                "session_id": self._session.get("session_id"),
                "turn": self._session.get("turn", 0),
                "agent_id": self._session.get("agent_id"),
                "model": self._session.get("model"),
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

        return json.dumps(info, indent=2)
