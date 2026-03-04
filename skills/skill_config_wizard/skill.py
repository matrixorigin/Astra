"""skill_config_wizard — guided skill configuration via conversation.

A meta-skill that reads a skill's manifest, checks what config is missing,
and returns a structured guide so the LLM can walk the user through setup.

The LLM then calls set_skill_setting / bind_skill_resource to apply values.
"""

from __future__ import annotations

from typing import Any

from pydantic import Field

from core.skills.base import (
    SideEffectCategory,
    SideEffectProfile,
    Skill,
    SkillInput,
    SkillOutput,
    SkillRequirement,
)


class ConfigWizardInput(SkillInput):
    """Input for the config wizard."""

    skill_name: str = Field(description="Name of the skill to configure")
    resource_key: str | None = Field(
        default=None,
        description="Optional resource key to configure (e.g. 'matrixorigin/matrixone')",
    )


class ConfigWizardOutput(SkillOutput):
    """Structured config guide for the LLM."""

    skill_name: str = ""
    valid: bool = False
    items: list[dict[str, Any]] = []
    missing_count: int = 0
    instructions: str = ""


class SkillConfigWizardSkill(Skill[ConfigWizardInput, ConfigWizardOutput]):
    """Guided skill configuration — reads manifest, checks status, returns setup guide.

    Use when the user says "configure github skill", "set up skill X",
    or "what config does skill Y need".
    """

    name = "skill_config_wizard"
    version = "1.0.0"
    description = (
        "Guide the user through configuring a skill. "
        "Shows what settings, secrets, and resource bindings are needed, "
        "what's already configured, and what's missing. "
        "Use when the user wants to set up or configure a skill."
    )
    requirements = SkillRequirement(timeout_seconds=10)
    side_effect_profile = SideEffectProfile(category=SideEffectCategory.READ)

    def __init__(self, db_factory=None, config_center=None) -> None:
        # db_factory is accepted for API compatibility but not used directly —
        # config_center is the only dependency. The executor injects config_center
        # lazily via get_config_center() when _config_center is None.
        self._config_center = config_center

    async def execute(self, inp: ConfigWizardInput) -> ConfigWizardOutput:
        if not self._config_center:
            return ConfigWizardOutput(
                success=False, error="Config center not available",
            )

        user_id = inp.user_id or ""
        skill_name = inp.skill_name

        # Validate to find what's missing
        errors = self._config_center.validate(
            skill_name, user_id, resource_key=inp.resource_key,
        )
        error_names = {(e.section, e.name, e.resource_key) for e in errors}

        # Resolve current config
        config = self._config_center.resolve_all(
            skill_name, user_id, resource_key=inp.resource_key,
        )

        # Load manifest for descriptions
        manifest = self._config_center.get_manifest(skill_name)
        if not manifest:
            return ConfigWizardOutput(
                success=False,
                error=f"No manifest found for skill '{skill_name}'",
                skill_name=skill_name,
            )

        items: list[dict[str, Any]] = []

        # Settings
        for s in manifest.get("settings", []):
            name = s["name"]
            is_missing = ("settings", name, None) in error_names
            current = config.settings.get(name)
            items.append({
                "name": name,
                "section": "settings",
                "description": s.get("description", ""),
                "type": s.get("type", "string"),
                "required": s.get("required", False),
                "has_default": "default" in s,
                "default_value": str(s["default"]) if "default" in s else None,
                "current_value": str(current) if current is not None else None,
                "status": "missing" if is_missing else ("ok" if current is not None or "default" in s else "optional"),
            })

        # Secrets
        for s in manifest.get("secrets", []):
            name = s["name"]
            is_missing = ("secrets", name, None) in error_names
            has_value = name in config.secrets
            items.append({
                "name": name,
                "section": "secrets",
                "description": s.get("description", ""),
                "type": "secret",
                "required": s.get("required", False),
                "has_default": "default" in s,
                "default_value": None,  # never expose secret defaults
                "current_value": "***" if has_value else None,
                "status": "missing" if is_missing else ("ok" if has_value else "optional"),
            })

        # Resource bindings
        res_spec = manifest.get("resources", {})
        if inp.resource_key and res_spec:
            for b in res_spec.get("bindings", []):
                bname = b["name"]
                is_missing = ("resources", bname, inp.resource_key) in error_names
                has_value = config.resource and bname in config.resource
                items.append({
                    "name": bname,
                    "section": "resources",
                    "description": b.get("description", ""),
                    "type": b.get("type", "string"),
                    "required": b.get("required", False),
                    "resource_key": inp.resource_key,
                    "has_default": "default" in b,
                    "default_value": str(b["default"]) if "default" in b and b.get("type") != "secret" else None,
                    "current_value": "***" if has_value and b.get("type") == "secret" else (str(config.resource[bname]) if has_value else None),
                    "status": "missing" if is_missing else ("ok" if has_value else "optional"),
                })

        missing = [i for i in items if i["status"] == "missing"]
        missing_count = len(missing)

        # Build LLM instructions
        if missing_count == 0:
            instructions = f"✅ Skill '{skill_name}' is fully configured."
        else:
            lines = [f"Skill '{skill_name}' needs {missing_count} value(s):"]
            for m in missing:
                sec = m["section"]
                hint = f" — {m['description']}" if m.get("description") else ""
                if sec == "secrets":
                    lines.append(f"  • Secret '{m['name']}'{hint}")
                    lines.append(f"    → Ask user for the value, then call: set_skill_setting('{skill_name}', '{m['name']}', '<value>')")
                elif sec == "resources":
                    lines.append(f"  • Resource binding '{m['name']}' for {m.get('resource_key', '?')}{hint}")
                    lines.append(f"    → Ask user, then call: bind_skill_resource('{skill_name}', '{m.get('resource_key')}', {{'{m['name']}': '<value>'}})")
                else:
                    lines.append(f"  • Setting '{m['name']}'{hint}")
                    lines.append(f"    → Ask user, then call: set_skill_setting('{skill_name}', '{m['name']}', '<value>')")
            instructions = "\n".join(lines)

        return ConfigWizardOutput(
            success=True,
            skill_name=skill_name,
            valid=missing_count == 0,
            items=items,
            missing_count=missing_count,
            instructions=instructions,
        )
