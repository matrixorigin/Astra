"""Memory program tool - Memoria backend."""

import json
from typing import Any, Dict, List

from cli.tools.base import EdgeTool, SideEffect


class MemoryProgramTool(EdgeTool):
    """Execute memory operations via Memoria."""

    name = "memory_program"
    description = (
        "Execute WRITE operations on memory (inject, correct, purge) via Memoria backend. "
        "Use ONLY when user explicitly wants to STORE/MODIFY/DELETE memories. "
        "For READING memories (e.g., 'what did I say about X?'), use memory_retrieve instead. "
        "Examples of when to use this tool: "
        "'remember that I prefer X', 'update my preference to Y', 'forget about Z'. "
        "Examples of when NOT to use: "
        "'what did I say about tests?', 'show me my preferences' (use memory_retrieve)."
    )
    parameters = {
        "type": "object",
        "properties": {
            "actions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "operation": {
                            "type": "string",
                            "enum": ["inject", "correct", "purge"],
                            "description": "Memory operation type",
                        },
                        "content": {
                            "type": "string",
                            "description": "Memory content to store or new content for correction",
                        },
                        "memory_type": {
                            "type": "string",
                            "enum": ["semantic", "profile", "procedural", "working", "tool_result"],
                            "default": "semantic",
                            "description": "Type of memory (optional, defaults to semantic)",
                        },
                        "memory_id": {
                            "type": "string",
                            "description": "Memory ID (required for correct; optional for purge if topic provided)",
                        },
                        "topic": {
                            "type": "string",
                            "description": "Keyword for bulk purge — deletes all memories matching this topic",
                        },
                        "reason": {
                            "type": "string",
                            "description": "Reason for correction or purge (optional)",
                        },
                    },
                    "required": ["operation"],
                },
            }
        },
        "required": ["actions"],
    }
    side_effect = SideEffect.WRITE

    async def execute(self, actions: List[Dict[str, Any]], **kwargs) -> str:
        """Execute memory actions via Memoria (sync HTTP wrapped in asyncio.to_thread)."""
        import asyncio

        def _run_sync() -> str:
            try:
                from core.memory.factory import create_editor

                user_id = kwargs.get("user_id", "default")
                session_id = kwargs.get("session_id")
                editor = create_editor(None, user_id=user_id)

                results = []
                for action in actions:
                    operation = action.get("operation")
                    content = action.get("content", "")

                    if operation == "inject":
                        memory_type = action.get("memory_type", "semantic")
                        editor.inject(
                            content=content,
                            memory_type=memory_type,
                            source="memory_program_tool",
                            session_id=session_id,
                        )
                        results.append({"operation": "inject", "status": "success"})

                    elif operation == "correct":
                        memory_id = action.get("memory_id")
                        reason = action.get("reason", "")
                        if not memory_id:
                            results.append(
                                {
                                    "operation": "correct",
                                    "status": "error",
                                    "error": "memory_id required",
                                }
                            )
                            continue
                        if not content:
                            results.append(
                                {
                                    "operation": "correct",
                                    "status": "error",
                                    "error": "content required for correct",
                                }
                            )
                            continue
                        editor.correct(memory_id, content, reason)
                        results.append({"operation": "correct", "status": "success"})

                    elif operation == "purge":
                        memory_id = action.get("memory_id")
                        topic = action.get("topic")
                        reason = action.get("reason", "")
                        if not memory_id and not topic:
                            results.append(
                                {
                                    "operation": "purge",
                                    "status": "error",
                                    "error": "memory_id or topic required",
                                }
                            )
                            continue
                        editor.purge(memory_id=memory_id, topic=topic, reason=reason)
                        results.append({"operation": "purge", "status": "success"})

                    else:
                        results.append(
                            {
                                "operation": operation,
                                "status": "error",
                                "error": f"Invalid operation: {operation}",
                            }
                        )

                return json.dumps(
                    {
                        "status": "success",
                        "actions_executed": len(results),
                        "results": results,
                    }
                )

            except Exception as e:
                return json.dumps(
                    {
                        "status": "error",
                        "error": str(e),
                        "actions_executed": 0,
                    }
                )

        return await asyncio.to_thread(_run_sync)
