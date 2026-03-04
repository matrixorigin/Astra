"""Tests for Config Center integration with SkillManifest and SkillManager.

Covers:
1. SkillManifest parses settings/secrets/resources from YAML
2. require_executable() validates config via config_center
3. End-to-end: register → install → configure → validate
"""

from datetime import datetime, timezone

import pytest
import yaml
from uuid_utils import uuid7

from api.models.skill import (
    SkillInstallation,
    SkillRegistry,
    SkillResourceBinding,
    SkillSetting,
)
from core.skills.config_center import SkillConfigCenter
from core.skills.credential_manager import CredentialManager
from core.skills.skill_manager import SkillConfigError, SkillManager, SkillNotInstalledError

GITHUB_MANIFEST = {
    "settings": [
        {"name": "base_url", "type": "string", "default": "https://api.github.com"},
        {"name": "instance_url", "type": "url", "required": True},
    ],
    "secrets": [
        {"name": "api_key", "required": True},
    ],
    "resources": {
        "type": "repo",
        "bindings": [
            {"name": "token", "type": "secret", "required": True},
        ],
    },
}


@pytest.fixture
def cred_mgr():
    return CredentialManager("test-key-for-integration")


@pytest.fixture(autouse=True)
def _clean(db):
    for model in (SkillResourceBinding, SkillSetting, SkillInstallation, SkillRegistry):
        db.query(model).delete(synchronize_session=False)
    db.commit()
    yield
    for model in (SkillResourceBinding, SkillSetting, SkillInstallation, SkillRegistry):
        db.query(model).delete(synchronize_session=False)
    db.commit()


def _register_and_install(db, skill_name: str = "github", manifest: dict | None = None) -> None:
    """Helper: register a marketplace skill and install for alice."""
    now = datetime.now(timezone.utc).replace(tzinfo=None)
    db.add(SkillRegistry(
        skill_id=f"{skill_name}@1.0.0",
        skill_name=skill_name,
        version="1.0.0",
        source="marketplace",
        is_active=1,
        is_public=1,
        status="active",
        manifest=manifest or GITHUB_MANIFEST,
        created_at=now,
    ))
    db.add(SkillInstallation(
        installation_id=str(uuid7()),
        user_id="alice",
        skill_name=skill_name,
        skill_version="1.0.0",
        status="installed",
        installed_at=now,
    ))
    db.commit()


# ---------------------------------------------------------------------------
# 1. SkillManifest YAML parsing
# ---------------------------------------------------------------------------


class TestManifestParsing:
    """SkillManifest parses settings/secrets/resources from YAML."""

    def test_parse_settings_secrets_resources(self, tmp_path):
        manifest_dir = tmp_path / "test_skill"
        manifest_dir.mkdir()
        (manifest_dir / "manifest.yaml").write_text(yaml.dump({
            "name": "test_skill",
            "version": "2.0.0",
            "settings": [
                {"name": "base_url", "type": "string", "default": "https://api.example.com"},
                {"name": "timeout", "type": "integer", "default": 30},
            ],
            "secrets": [
                {"name": "api_token", "required": True},
            ],
            "resources": {
                "type": "project",
                "bindings": [{"name": "token", "type": "secret", "required": True}],
            },
        }))

        from core.skills.loader import load_manifests
        manifests = load_manifests(tmp_path)

        assert len(manifests) == 1
        m = manifests[0]
        assert m.name == "test_skill"
        assert m.version == "2.0.0"
        assert len(m.settings) == 2
        assert m.settings[0]["name"] == "base_url"
        assert m.settings[0]["default"] == "https://api.example.com"
        assert len(m.secrets) == 1
        assert m.secrets[0]["name"] == "api_token"
        assert m.secrets[0]["required"] is True
        assert m.resources["type"] == "project"
        assert len(m.resources["bindings"]) == 1

    def test_parse_minimal_manifest(self, tmp_path):
        """Manifest with only name/version — no settings/secrets/resources."""
        manifest_dir = tmp_path / "minimal"
        manifest_dir.mkdir()
        (manifest_dir / "manifest.yaml").write_text(yaml.dump({
            "name": "minimal",
            "version": "1.0.0",
        }))

        from core.skills.loader import load_manifests
        manifests = load_manifests(tmp_path)

        assert len(manifests) == 1
        m = manifests[0]
        assert m.settings == []
        assert m.secrets == []
        assert m.resources == {}


# ---------------------------------------------------------------------------
# 2. require_executable() config validation
# ---------------------------------------------------------------------------


class TestRequireExecutableConfigValidation:
    """require_executable() rejects when required config is missing."""

    def test_rejects_missing_config_with_real_center(self, db, db_factory, cred_mgr):
        """End-to-end: real config center, real DB, missing required config → SkillConfigError."""
        _register_and_install(db)

        center = SkillConfigCenter(
            db_factory=db_factory,
            credential_mgr=cred_mgr,
            manifest_loader=lambda name: GITHUB_MANIFEST if name == "github" else None,
        )
        mgr = SkillManager(db_factory, cred_mgr, config_center=center)

        with pytest.raises(SkillConfigError, match=r"missing required config.*settings\.instance_url"):
            mgr.require_executable("alice", "github")

    def test_passes_when_config_satisfied(self, db, db_factory, cred_mgr):
        """All required config set → require_executable succeeds."""
        _register_and_install(db)

        center = SkillConfigCenter(
            db_factory=db_factory,
            credential_mgr=cred_mgr,
            manifest_loader=lambda name: GITHUB_MANIFEST if name == "github" else None,
        )
        center.set_setting("github", "instance_url", "https://gh.corp.com",
                           scope_type="user", scope_id="alice", updated_by="alice")
        center.set_setting("github", "api_key", "key_abc",
                           scope_type="user", scope_id="alice", updated_by="alice")

        mgr = SkillManager(db_factory, cred_mgr, config_center=center)
        mgr.require_executable("alice", "github")  # no exception

    def test_no_config_center_skips_validation(self, db, db_factory, cred_mgr):
        """Without config_center, require_executable still works."""
        _register_and_install(db)

        mgr = SkillManager(db_factory, cred_mgr, config_center=None)
        mgr.require_executable("alice", "github")  # no exception

    def test_builtin_skill_skips_config_validation(self, db, db_factory, cred_mgr):
        """Builtin skills skip all checks including config validation."""
        # No skill in registry → treated as builtin
        center = SkillConfigCenter(
            db_factory=db_factory,
            credential_mgr=cred_mgr,
            manifest_loader=lambda name: GITHUB_MANIFEST if name == "github" else None,
        )
        mgr = SkillManager(db_factory, cred_mgr, config_center=center)
        mgr.require_executable("alice", "unknown_builtin")  # no exception

    def test_config_error_is_not_not_installed_error(self, db, db_factory, cred_mgr):
        """SkillConfigError is distinct from SkillNotInstalledError."""
        _register_and_install(db)

        center = SkillConfigCenter(
            db_factory=db_factory,
            credential_mgr=cred_mgr,
            manifest_loader=lambda name: GITHUB_MANIFEST if name == "github" else None,
        )
        mgr = SkillManager(db_factory, cred_mgr, config_center=center)

        with pytest.raises(SkillConfigError) as exc_info:
            mgr.require_executable("alice", "github")

        # Verify it's NOT a SkillNotInstalledError
        assert not isinstance(exc_info.value, SkillNotInstalledError)


# ---------------------------------------------------------------------------
# 3. End-to-end: register → install → configure → resolve
# ---------------------------------------------------------------------------


class TestEndToEndConfigFlow:
    """Full lifecycle: skill registered, installed, configured, resolved."""

    def test_full_lifecycle(self, db, db_factory, cred_mgr):
        """Register → install → set config → resolve_all → validate passes."""
        _register_and_install(db)

        center = SkillConfigCenter(
            db_factory=db_factory,
            credential_mgr=cred_mgr,
            manifest_loader=lambda name: GITHUB_MANIFEST if name == "github" else None,
        )

        # Step 1: Validation fails — required config missing
        errors = center.validate("github", "alice")
        assert len(errors) == 2  # instance_url + api_key
        names = {e.name for e in errors}
        assert names == {"instance_url", "api_key"}

        # Step 2: Set required config
        center.set_setting("github", "instance_url", "https://gh.corp.com",
                           scope_type="user", scope_id="alice", updated_by="alice")
        center.set_setting("github", "api_key", "secret_key_123",
                           scope_type="user", scope_id="alice", updated_by="alice")

        # Step 3: Validation passes
        errors = center.validate("github", "alice")
        assert errors == []

        # Step 4: resolve_all returns complete config
        config = center.resolve_all("github", "alice")
        assert config.settings["base_url"] == "https://api.github.com"  # manifest default
        assert config.settings["instance_url"] == "https://gh.corp.com"
        assert config.secrets["api_key"] == "secret_key_123"

        # Step 5: require_executable passes
        mgr = SkillManager(db_factory, cred_mgr, config_center=center)
        mgr.require_executable("alice", "github")  # no exception

        # Step 6: Verify DB ground truth — all settings persisted
        rows = db.query(SkillSetting).filter(
            SkillSetting.skill_name == "github",
            SkillSetting.scope_id == "alice",
        ).all()
        assert len(rows) == 2
        setting_names = {r.setting_name for r in rows}
        assert setting_names == {"instance_url", "api_key"}
        for r in rows:
            assert r.scope_type == "user"
            assert r.updated_by == "alice"
            assert r.created_at is not None
            assert r.updated_at is not None
