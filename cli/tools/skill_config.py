"""Edge tools for skill configuration — callable by LLM in conversation.

These tools wrap the REST API so the LLM can set settings, bind resources,
and validate config during a guided configuration flow.
"""

from __future__ import annotations

import json
from typing import Any

from cli.tools.base import EdgeTool, SideEffect


class SetSkillSettingTool(EdgeTool):
    name = "set_skill_setting"
    description = (
        "Set a skill configuration value (setting or secret). "
        "Use after skill_config_wizard identifies missing config. "
        "For GitHub skills, always use skill_name='github' — setting it once covers all GitHub skills."
    )
    parameters = {
        "type": "object",
        "properties": {
            "skill_name": {"type": "string", "description": "Skill to configure"},
            "setting_name": {"type": "string", "description": "Setting key"},
            "value": {"type": "string", "description": "Value to set"},
        },
        "required": ["skill_name", "setting_name", "value"],
    }
    side_effect = SideEffect.WRITE

    def __init__(self, api_client: Any) -> None:
        self._api = api_client

    async def execute(self, **kwargs: Any) -> str:
        skill = kwargs["skill_name"]
        key = kwargs["setting_name"]
        val = kwargs["value"]
        try:
            await self._api.set_skill_setting(skill, key, val)
            return json.dumps({"success": True, "message": f"{skill}.{key} set"})
        except Exception as e:
            return json.dumps({"success": False, "error": str(e)})


class BindSkillResourceTool(EdgeTool):
    name = "bind_skill_resource"
    description = (
        "Bind credentials to a specific resource instance (e.g. a GitHub repo). "
        "Pass bindings as key=value pairs."
    )
    parameters = {
        "type": "object",
        "properties": {
            "skill_name": {"type": "string", "description": "Skill name"},
            "resource_key": {"type": "string", "description": "Resource identifier (e.g. 'owner/repo')"},
            "bindings": {
                "type": "object",
                "description": "Key-value pairs to bind (e.g. {\"read_token\": \"ghp_...\"})",
            },
        },
        "required": ["skill_name", "resource_key", "bindings"],
    }
    side_effect = SideEffect.WRITE

    def __init__(self, api_client: Any) -> None:
        self._api = api_client

    async def execute(self, **kwargs: Any) -> str:
        try:
            await self._api.bind_skill_resource(
                kwargs["skill_name"], kwargs["resource_key"], kwargs["bindings"],
            )
            return json.dumps({"success": True, "resource_key": kwargs["resource_key"]})
        except Exception as e:
            return json.dumps({"success": False, "error": str(e)})


class ValidateSkillConfigTool(EdgeTool):
    name = "validate_skill_config"
    description = "Check if a skill has all required configuration set."
    parameters = {
        "type": "object",
        "properties": {
            "skill_name": {"type": "string", "description": "Skill to validate"},
            "resource": {"type": "string", "description": "Optional resource key to validate"},
        },
        "required": ["skill_name"],
    }
    side_effect = SideEffect.READ

    def __init__(self, api_client: Any) -> None:
        self._api = api_client

    async def execute(self, **kwargs: Any) -> str:
        try:
            result = await self._api.validate_skill_config(
                kwargs["skill_name"], resource=kwargs.get("resource"),
            )
            return json.dumps(result)
        except Exception as e:
            return json.dumps({"success": False, "error": str(e)})


class SkillConfigWizardTool(EdgeTool):
    """Edge tool wrapper for skill_config_wizard — calls the REST API."""

    name = "skill_config_wizard"
    description = (
        "Show what configuration a skill needs and what's already set. "
        "Call this when the user explicitly asks to configure a skill, or when a skill call fails due to missing config. "
        "Do NOT call this proactively before using a skill — just call the skill directly. "
        "For GitHub skills (summarize_pr, list_prs, ci_status, list_issues, get_issue, create_issue), "
        "use skill_name='github' — they all share one token. "
        "Returns missing required fields and instructions for what to set."
    )
    parameters = {
        "type": "object",
        "properties": {
            "skill_name": {"type": "string", "description": "Name of the skill to configure"},
            "resource_key": {
                "type": "string",
                "description": "Optional resource key (e.g. 'owner/repo') to check resource bindings",
            },
        },
        "required": ["skill_name"],
    }
    side_effect = SideEffect.READ

    def __init__(self, api_client: Any) -> None:
        self._api = api_client

    async def execute(self, **kwargs: Any) -> str:
        skill_name = kwargs["skill_name"]
        resource_key = kwargs.get("resource_key")
        try:
            # GET /skills/{skill_name}/config
            config = await self._api.get_skill_config(skill_name)
            # GET /skills/{skill_name}/config/validate
            validation = await self._api.validate_skill_config(skill_name, resource=resource_key)
            resources = await self._api.list_skill_resources(skill_name)

            missing = validation.get("errors", [])
            return json.dumps({
                "skill_name": skill_name,
                "valid": validation.get("valid", False),
                "missing_count": len(missing),
                "missing": missing,
                "current_settings": config.get("settings", {}),
                "secrets_configured": list(config.get("secrets", {}).keys()),
                "resources_configured": resources,
                "instructions": (
                    f"✅ {skill_name} is fully configured." if not missing
                    else f"Need to set {len(missing)} value(s): "
                         + ", ".join(f"{e['section']}.{e['name']}" for e in missing)
                         + f". Ask the user for each value, then call set_skill_setting"
                         + f" with skill_name='{skill_name}' (set once — all related skills share this config)."
                ),
            })
        except Exception as e:
            return json.dumps({"success": False, "error": str(e), "skill_name": skill_name})


def register_skill_config_tools(router: Any, api_client: Any) -> None:
    """Register all skill config tools on the ToolRouter."""
    router.register(SkillConfigWizardTool(api_client))
    router.register(SetSkillSettingTool(api_client))
    router.register(BindSkillResourceTool(api_client))
    router.register(ValidateSkillConfigTool(api_client))
