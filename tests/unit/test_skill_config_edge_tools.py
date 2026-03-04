"""Unit tests for skill config edge tools.

Integration tests (real API + real DB) live in:
  tests/integration/test_skill_config_api.py::TestEdgeToolsIntegration

This file covers things that don't need a real API:
- SkillLoader must not overwrite the registered edge tool
- github manifest must use new secrets: format
"""

import pytest

from cli.tools.skill_config import SkillConfigWizardTool, register_skill_config_tools
from cli.tools.router import ToolRouter
from unittest.mock import MagicMock


class TestSkillLoaderDoesNotOverwriteEdgeTool:
    def test_tool_router_register_overwrites(self):
        """ToolRouter.register() always overwrites — callers must guard against this.

        This documents the ToolRouter contract: it does NOT protect against
        duplicate registration. The production guard lives in mo_agent_api.py:
          if local.skill.name in builtin_names: continue
        This test verifies that contract so any future change to ToolRouter
        that adds auto-dedup doesn't silently change behavior.
        """
        router = ToolRouter()
        tool_a = MagicMock()
        tool_a.name = "skill_config_wizard"
        tool_b = MagicMock()
        tool_b.name = "skill_config_wizard"

        router.register(tool_a)
        router.register(tool_b)

        assert router.get_tool("skill_config_wizard") is tool_b, \
            "ToolRouter.register() must overwrite — production guard is in mo_agent_api.py"

    def test_production_guard_prevents_overwrite(self):
        """The production guard in mo_agent_api.py must skip skills already registered."""
        import os
        from core.skills.loader import SkillLoader

        router = ToolRouter()
        register_skill_config_tools(router, MagicMock())

        original = router.get_tool("skill_config_wizard")
        assert isinstance(original, SkillConfigWizardTool)

        # Simulate the production guard from mo_agent_api.py
        builtin_names = {t.name for t in router.list_tools()}
        for local in SkillLoader.discover(SkillLoader.default_paths(os.getcwd())):
            if local.skill.name in builtin_names:
                continue  # ← this is the guard we're testing
            router.register(local.skill)

        assert router.get_tool("skill_config_wizard") is original, \
            "Production guard must prevent skills/ directory from overwriting edge tools"


class TestGithubManifestFormat:
    def test_manifest_has_secrets_not_credentials(self):
        """github/manifest.yaml must use new secrets: format, not legacy credentials:."""
        import yaml
        from pathlib import Path

        manifest = yaml.safe_load(
            (Path(__file__).parent.parent.parent / "skills/github/manifest.yaml").read_text()
        )
        assert "secrets" in manifest, "manifest.yaml must use 'secrets:' format"
        assert "credentials" not in manifest, "legacy 'credentials:' key found"

        secret_names = [s["name"] for s in manifest["secrets"]]
        assert "github_token" in secret_names

        token = next(s for s in manifest["secrets"] if s["name"] == "github_token")
        assert token.get("required") is True

    def test_manifest_parsed_by_config_center(self, db_factory):
        """Config center must find github_token as a required secret."""
        from core.skills.config_center import SkillConfigCenter
        from core.skills.credential_manager import CredentialManager
        from core.skills.loader import load_manifests

        manifests = {m.name: m for m in load_manifests()}
        github_manifest = manifests.get("github")
        assert github_manifest is not None, "github manifest not loaded"

        cred_mgr = CredentialManager("test-key")
        center = SkillConfigCenter(
            db_factory, cred_mgr,
            manifest_loader=lambda n: vars(github_manifest) if n == "github" else None,
        )
        errors = center.validate("github", "test-user")
        error_names = {e.name for e in errors}
        assert "github_token" in error_names, \
            "github_token not flagged as missing — manifest format not recognized"
