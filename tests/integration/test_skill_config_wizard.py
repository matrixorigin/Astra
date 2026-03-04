"""Integration tests for skill_config_wizard meta-skill."""

import pytest

from api.models.skill import SkillResourceBinding, SkillSetting
from core.skills.config_center import SkillConfigCenter
from core.skills.credential_manager import CredentialManager
from skills.skill_config_wizard.skill import ConfigWizardInput, SkillConfigWizardSkill

GITHUB_MANIFEST = {
    "name": "github",
    "version": "1.0.0",
    "settings": [
        {"name": "api_base_url", "type": "string", "default": "https://api.github.com"},
        {"name": "instance_url", "type": "url", "required": True, "description": "GitHub Enterprise URL"},
    ],
    "secrets": [
        {"name": "api_key", "description": "GitHub API token", "required": True},
    ],
    "resources": {
        "type": "repo",
        "bindings": [
            {"name": "read_token", "type": "secret", "required": True, "description": "Repo access token"},
            {"name": "default_branch", "type": "string", "default": "main"},
        ],
    },
}


@pytest.fixture
def center(db_factory):
    cred_mgr = CredentialManager("test-wizard-key")
    return SkillConfigCenter(
        db_factory, cred_mgr,
        manifest_loader=lambda n: GITHUB_MANIFEST if n == "github" else None,
    )


@pytest.fixture
def wizard(db_factory, center):
    return SkillConfigWizardSkill(db_factory=db_factory, config_center=center)


@pytest.fixture(autouse=True)
def _clean(db):
    """Clean only settings/bindings created by this test module (user 'alice')."""
    yield
    from sqlalchemy import delete
    db.execute(
        delete(SkillResourceBinding).where(SkillResourceBinding.user_id == "alice")
    )
    db.execute(
        delete(SkillSetting).where(SkillSetting.scope_id == "alice")
    )
    db.commit()


@pytest.mark.asyncio
async def test_unconfigured_skill_shows_missing(wizard):
    """Fresh skill → wizard reports missing required items."""
    out = await wizard.execute(ConfigWizardInput(
        skill_name="github", user_id="alice",
    ))
    assert out.success is True
    assert out.valid is False
    assert out.missing_count == 2  # instance_url + api_key

    missing_names = {i["name"] for i in out.items if i["status"] == "missing"}
    assert missing_names == {"instance_url", "api_key"}

    # api_base_url has default → status "ok"
    base_url = next(i for i in out.items if i["name"] == "api_base_url")
    assert base_url["status"] == "ok"
    assert base_url["default_value"] == "https://api.github.com"

    # Instructions mention how to set
    assert "set_skill_setting" in out.instructions


@pytest.mark.asyncio
async def test_fully_configured_skill(wizard, center):
    """After setting all required → wizard reports valid."""
    center.set_setting("github", "instance_url", "https://gh.corp.com",
                       scope_type="user", scope_id="alice")
    center.set_setting("github", "api_key", "sk-123",
                       scope_type="user", scope_id="alice")

    out = await wizard.execute(ConfigWizardInput(
        skill_name="github", user_id="alice",
    ))
    assert out.success is True
    assert out.valid is True
    assert out.missing_count == 0
    assert "fully configured" in out.instructions


@pytest.mark.asyncio
async def test_resource_bindings_shown(wizard, center):
    """With resource_key, wizard shows resource binding status."""
    center.set_setting("github", "instance_url", "https://gh.corp.com",
                       scope_type="user", scope_id="alice")
    center.set_setting("github", "api_key", "sk-123",
                       scope_type="user", scope_id="alice")

    out = await wizard.execute(ConfigWizardInput(
        skill_name="github", user_id="alice",
        resource_key="matrixorigin/matrixone",
    ))
    assert out.valid is False
    assert out.missing_count == 1  # read_token missing

    rt = next(i for i in out.items if i["name"] == "read_token")
    assert rt["status"] == "missing"
    assert rt["section"] == "resources"

    # default_branch has default → ok
    db_item = next(i for i in out.items if i["name"] == "default_branch")
    assert db_item["status"] == "ok"

    assert "bind_skill_resource" in out.instructions


@pytest.mark.asyncio
async def test_unknown_skill_returns_error(wizard):
    """Unknown skill → error with clear message."""
    out = await wizard.execute(ConfigWizardInput(
        skill_name="nonexistent", user_id="alice",
    ))
    assert out.success is False
    assert "No manifest found" in out.error


@pytest.mark.asyncio
async def test_secrets_masked_in_output(wizard, center):
    """Secret values shown as *** in wizard output."""
    center.set_setting("github", "instance_url", "https://gh.corp.com",
                       scope_type="user", scope_id="alice")
    center.set_setting("github", "api_key", "super-secret-key",
                       scope_type="user", scope_id="alice")

    out = await wizard.execute(ConfigWizardInput(
        skill_name="github", user_id="alice",
    ))
    api_key_item = next(i for i in out.items if i["name"] == "api_key")
    assert api_key_item["current_value"] == "***"
    assert api_key_item["status"] == "ok"


@pytest.mark.asyncio
async def test_real_github_manifest_github_token_required(db_factory):
    """Wizard against the real skills/github/manifest.yaml must flag github_token as missing."""
    import yaml
    from pathlib import Path
    from core.skills.credential_manager import CredentialManager

    manifest_path = Path(__file__).parent.parent.parent / "skills" / "github" / "manifest.yaml"
    assert manifest_path.exists(), f"manifest not found at {manifest_path}"
    real_manifest = yaml.safe_load(manifest_path.read_text())

    cred_mgr = CredentialManager("test-real-manifest-key")
    real_center = SkillConfigCenter(
        db_factory, cred_mgr,
        manifest_loader=lambda n: real_manifest if n == "github" else None,
    )
    real_wizard = SkillConfigWizardSkill(db_factory=db_factory, config_center=real_center)

    out = await real_wizard.execute(ConfigWizardInput(skill_name="github", user_id="alice"))
    assert out.success is True
    assert out.valid is False

    missing_names = {i["name"] for i in out.items if i["status"] == "missing"}
    assert "github_token" in missing_names, (
        f"github_token not flagged as missing. Missing: {missing_names}. "
        "Check skills/github/manifest.yaml secrets section."
    )
