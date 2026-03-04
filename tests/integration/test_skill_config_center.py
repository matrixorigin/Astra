"""Integration tests for Skill Configuration Center (§13).

Design intent verified:
- Settings: scope chain resolution (user → tenant → global → manifest default)
- Secrets: encrypted storage, transparent decrypt on read
- Resource bindings: per-user per-resource, fallback to skill-level secret
- Validation: required fields, missing config detection
- resolve_all: single call returns complete SkillConfig for execution
- Scope validation: invalid scope_type / missing scope_id rejected
"""

import pytest

from api.models.skill import SkillResourceBinding, SkillSetting
from core.skills.config_center import ConfigValidationError, SkillConfig, SkillConfigCenter
from core.skills.credential_manager import CredentialManager

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

GITHUB_MANIFEST = {
    "name": "github",
    "version": "1.0.0",
    "settings": [
        {"name": "api_base_url", "type": "string", "default": "https://api.github.com"},
        {"name": "timeout", "type": "integer", "default": 30},
        {"name": "instance_url", "type": "url", "required": True},  # required, no default
    ],
    "secrets": [
        {"name": "default_token", "description": "Fallback token", "required": False},
        {"name": "api_key", "description": "Required API key", "required": True},
    ],
    "resources": {
        "type": "repo",
        "key_pattern": "{owner}/{name}",
        "bindings": [
            {"name": "read_token", "type": "secret", "required": True},
            {"name": "write_token", "type": "secret", "required": False},
            {"name": "default_branch", "type": "string", "default": "main"},
        ],
    },
}


@pytest.fixture
def cred_mgr():
    return CredentialManager("test-secret-key-for-config-center")


@pytest.fixture
def center(db_factory, cred_mgr):
    def manifest_loader(skill_name: str) -> dict | None:
        if skill_name == "github":
            return GITHUB_MANIFEST
        return None

    return SkillConfigCenter(
        db_factory=db_factory,
        credential_mgr=cred_mgr,
        manifest_loader=manifest_loader,
    )


@pytest.fixture(autouse=True)
def _clean(db):
    db.query(SkillResourceBinding).delete(synchronize_session=False)
    db.query(SkillSetting).delete(synchronize_session=False)
    db.commit()
    yield
    db.query(SkillResourceBinding).delete(synchronize_session=False)
    db.query(SkillSetting).delete(synchronize_session=False)
    db.commit()


# ---------------------------------------------------------------------------
# 1. Settings — scope chain resolution
# ---------------------------------------------------------------------------


class TestSettingScopeChain:
    """Settings resolve through user → tenant → global → manifest default."""

    def test_manifest_default(self, center):
        """No DB rows → manifest default returned."""
        val = center.get_setting("github", "api_base_url", user_id="alice")
        assert val == "https://api.github.com"

    def test_global_overrides_manifest(self, center):
        """Global scope overrides manifest default."""
        center.set_setting("github", "api_base_url", "https://global.example.com",
                           scope_type="global", updated_by="admin")

        val = center.get_setting("github", "api_base_url", user_id="alice")
        assert val == "https://global.example.com"

    def test_tenant_overrides_global(self, center):
        """Tenant scope overrides global."""
        center.set_setting("github", "api_base_url", "https://global.example.com",
                           scope_type="global", updated_by="admin")
        center.set_setting("github", "api_base_url", "https://tenant.corp.com",
                           scope_type="tenant", scope_id="acme", updated_by="admin")

        val = center.get_setting("github", "api_base_url", user_id="alice", tenant_id="acme")
        assert val == "https://tenant.corp.com"

    def test_user_overrides_tenant(self, center):
        """User scope overrides tenant."""
        center.set_setting("github", "api_base_url", "https://tenant.corp.com",
                           scope_type="tenant", scope_id="acme", updated_by="admin")
        center.set_setting("github", "api_base_url", "https://alice.dev",
                           scope_type="user", scope_id="alice", updated_by="alice")

        val = center.get_setting("github", "api_base_url", user_id="alice", tenant_id="acme")
        assert val == "https://alice.dev"

    def test_full_chain_user_wins(self, center):
        """All three scopes set — user wins."""
        center.set_setting("github", "timeout", "10",
                           scope_type="global", updated_by="admin")
        center.set_setting("github", "timeout", "20",
                           scope_type="tenant", scope_id="acme", updated_by="admin")
        center.set_setting("github", "timeout", "30",
                           scope_type="user", scope_id="alice", updated_by="alice")

        assert center.get_setting("github", "timeout", user_id="alice", tenant_id="acme") == "30"

    def test_tenant_skipped_when_no_tenant_id(self, center):
        """Without tenant_id, tenant scope is skipped entirely."""
        center.set_setting("github", "timeout", "20",
                           scope_type="tenant", scope_id="acme", updated_by="admin")

        # No tenant_id → falls through to manifest default (30)
        val = center.get_setting("github", "timeout", user_id="alice")
        assert val == 30  # manifest default, not tenant value

    def test_missing_setting_returns_none(self, center):
        """Setting not in manifest and not in DB → None."""
        val = center.get_setting("github", "nonexistent", user_id="alice")
        assert val is None

    def test_upsert_overwrites(self, db, center):
        """Setting same scope twice → updates, not duplicates."""
        center.set_setting("github", "timeout", "60",
                           scope_type="user", scope_id="alice", updated_by="alice")
        center.set_setting("github", "timeout", "90",
                           scope_type="user", scope_id="alice", updated_by="alice")

        val = center.get_setting("github", "timeout", user_id="alice")
        assert val == "90"

        count = db.query(SkillSetting).filter(
            SkillSetting.skill_name == "github",
            SkillSetting.setting_name == "timeout",
            SkillSetting.scope_type == "user",
            SkillSetting.scope_id == "alice",
        ).count()
        assert count == 1

    def test_upsert_updates_updated_at(self, db, center):
        """Upsert updates updated_at timestamp."""
        center.set_setting("github", "timeout", "60",
                           scope_type="user", scope_id="alice", updated_by="alice")
        row1 = db.query(SkillSetting).filter(
            SkillSetting.setting_name == "timeout",
        ).one()
        ts1 = row1.updated_at

        center.set_setting("github", "timeout", "90",
                           scope_type="user", scope_id="alice", updated_by="bob")
        db.expire_all()
        row2 = db.query(SkillSetting).filter(
            SkillSetting.setting_name == "timeout",
        ).one()
        assert row2.setting_value == "90"
        assert row2.updated_by == "bob"
        # updated_at should be >= first write (may be equal if sub-second)
        assert row2.updated_at >= ts1

    def test_delete_setting(self, center):
        """Delete removes the row, fallback kicks in."""
        center.set_setting("github", "api_base_url", "https://custom.com",
                           scope_type="user", scope_id="alice", updated_by="alice")
        assert center.get_setting("github", "api_base_url", user_id="alice") == "https://custom.com"

        deleted = center.delete_setting("github", "api_base_url", scope_type="user", scope_id="alice")
        assert deleted is True

        # Falls back to manifest default
        assert center.get_setting("github", "api_base_url", user_id="alice") == "https://api.github.com"

    def test_delete_nonexistent_returns_false(self, center):
        """Deleting a setting that doesn't exist returns False."""
        deleted = center.delete_setting("github", "nonexistent", scope_type="user", scope_id="alice")
        assert deleted is False

    def test_setting_db_fields(self, db, center):
        """Verify every field persisted in skill_settings."""
        center.set_setting("github", "timeout", "45",
                           scope_type="tenant", scope_id="acme", updated_by="admin")

        row = db.query(SkillSetting).filter(
            SkillSetting.skill_name == "github",
            SkillSetting.setting_name == "timeout",
        ).one()
        assert row.setting_id is not None
        assert len(row.setting_id) == 36  # uuid7 format
        assert row.skill_name == "github"
        assert row.setting_name == "timeout"
        assert row.setting_value == "45"
        assert row.is_secret == 0
        assert row.scope_type == "tenant"
        assert row.scope_id == "acme"
        assert row.updated_by == "admin"
        assert row.created_at is not None
        assert row.updated_at is not None

    def test_different_scopes_are_independent(self, db, center):
        """Same setting at different scopes creates separate rows."""
        center.set_setting("github", "timeout", "10",
                           scope_type="global", updated_by="admin")
        center.set_setting("github", "timeout", "20",
                           scope_type="user", scope_id="alice", updated_by="alice")

        count = db.query(SkillSetting).filter(
            SkillSetting.skill_name == "github",
            SkillSetting.setting_name == "timeout",
        ).count()
        assert count == 2


# ---------------------------------------------------------------------------
# 2. Scope validation
# ---------------------------------------------------------------------------


class TestScopeValidation:
    """Invalid scope_type or missing scope_id is rejected."""

    def test_invalid_scope_type_set(self, center):
        with pytest.raises(ValueError, match="Invalid scope_type"):
            center.set_setting("github", "timeout", "60",
                               scope_type="invalid", scope_id="x", updated_by="alice")

    def test_user_scope_requires_scope_id(self, center):
        with pytest.raises(ValueError, match="scope_id is required"):
            center.set_setting("github", "timeout", "60",
                               scope_type="user", scope_id=None, updated_by="alice")

    def test_tenant_scope_requires_scope_id(self, center):
        with pytest.raises(ValueError, match="scope_id is required"):
            center.set_setting("github", "timeout", "60",
                               scope_type="tenant", scope_id=None, updated_by="alice")

    def test_global_scope_allows_none_scope_id(self, center):
        # Should not raise
        center.set_setting("github", "timeout", "60",
                           scope_type="global", updated_by="admin")
        assert center.get_setting("github", "timeout", user_id="alice") == "60"

    def test_invalid_scope_type_delete(self, center):
        with pytest.raises(ValueError, match="Invalid scope_type"):
            center.delete_setting("github", "timeout", scope_type="bad")


# ---------------------------------------------------------------------------
# 3. Secrets — encrypted storage
# ---------------------------------------------------------------------------


class TestSecrets:
    """Secrets are encrypted in DB, decrypted on read."""

    def test_secret_encrypted_in_db(self, db, center, cred_mgr):
        """Secret value is encrypted in skill_settings, decrypted on get."""
        center.set_setting("github", "default_token", "ghp_secret_123",
                           scope_type="user", scope_id="alice", updated_by="alice")

        # Raw DB value is encrypted
        row = db.query(SkillSetting).filter(
            SkillSetting.skill_name == "github",
            SkillSetting.setting_name == "default_token",
        ).one()
        assert row.is_secret == 1
        assert row.setting_value != "ghp_secret_123"
        assert cred_mgr.decrypt(row.setting_value) == "ghp_secret_123"

        # get_setting returns decrypted
        val = center.get_setting("github", "default_token", user_id="alice")
        assert val == "ghp_secret_123"

    def test_plaintext_not_encrypted(self, db, center):
        """Non-secret settings are stored as plaintext."""
        center.set_setting("github", "timeout", "60",
                           scope_type="user", scope_id="alice", updated_by="alice")

        row = db.query(SkillSetting).filter(
            SkillSetting.setting_name == "timeout",
        ).one()
        assert row.is_secret == 0
        assert row.setting_value == "60"

    def test_secret_scope_chain(self, center):
        """Secrets follow the same scope chain as settings."""
        center.set_setting("github", "default_token", "ghp_global",
                           scope_type="global", updated_by="admin")
        center.set_setting("github", "default_token", "ghp_alice",
                           scope_type="user", scope_id="alice", updated_by="alice")

        assert center.get_setting("github", "default_token", user_id="alice") == "ghp_alice"
        assert center.get_setting("github", "default_token", user_id="bob") == "ghp_global"


# ---------------------------------------------------------------------------
# 4. Resource Bindings — per-user per-resource
# ---------------------------------------------------------------------------


class TestResourceBindings:
    """Resource bindings are per-user, per-resource, with fallback to skill secret."""

    def test_bind_and_get_all_fields(self, db, center, cred_mgr):
        """bind_resource stores all fields correctly, get retrieves decrypted."""
        center.bind_resource("alice", "github", "matrixorigin/mo", {
            "read_token": "ghp_read",
            "default_branch": "dev",
        })

        # Secret binding: encrypted in DB
        row = db.query(SkillResourceBinding).filter(
            SkillResourceBinding.binding_name == "read_token",
        ).one()
        assert row.binding_id is not None
        assert len(row.binding_id) == 36
        assert row.user_id == "alice"
        assert row.skill_name == "github"
        assert row.resource_type == "repo"
        assert row.resource_key == "matrixorigin/mo"
        assert row.binding_name == "read_token"
        assert row.is_secret == 1
        assert row.binding_value != "ghp_read"
        assert cred_mgr.decrypt(row.binding_value) == "ghp_read"
        assert row.created_at is not None
        assert row.updated_at is not None

        # Plaintext binding
        row2 = db.query(SkillResourceBinding).filter(
            SkillResourceBinding.binding_name == "default_branch",
        ).one()
        assert row2.is_secret == 0
        assert row2.binding_value == "dev"

        # get returns decrypted
        assert center.get_resource_binding("alice", "github", "matrixorigin/mo", "read_token") == "ghp_read"
        assert center.get_resource_binding("alice", "github", "matrixorigin/mo", "default_branch") == "dev"

    def test_fallback_to_skill_secret(self, center):
        """Missing resource binding falls back to skill-level secret."""
        center.set_setting("github", "default_token", "ghp_fallback",
                           scope_type="user", scope_id="alice", updated_by="alice")

        val = center.get_resource_binding("alice", "github", "unknown/repo", "default_token")
        assert val == "ghp_fallback"

    def test_no_fallback_returns_none(self, center):
        """No binding and no fallback → None."""
        val = center.get_resource_binding("alice", "github", "unknown/repo", "read_token")
        assert val is None

    def test_unbind_resource(self, db, center):
        """unbind_resource removes all bindings for a resource."""
        center.bind_resource("alice", "github", "org/repo", {
            "read_token": "tok1", "default_branch": "dev",
        })
        assert db.query(SkillResourceBinding).filter(
            SkillResourceBinding.resource_key == "org/repo",
        ).count() == 2

        count = center.unbind_resource("alice", "github", "org/repo")
        assert count == 2
        assert db.query(SkillResourceBinding).filter(
            SkillResourceBinding.resource_key == "org/repo",
        ).count() == 0

    def test_unbind_nonexistent_returns_zero(self, center):
        """Unbinding a resource that doesn't exist returns 0."""
        count = center.unbind_resource("alice", "github", "no/such/repo")
        assert count == 0

    def test_list_resources(self, center):
        """list_resources returns distinct resource keys."""
        center.bind_resource("alice", "github", "org/repo1", {"read_token": "t1"})
        center.bind_resource("alice", "github", "org/repo2", {"read_token": "t2"})

        resources = center.list_resources("alice", "github")
        keys = {r["resource_key"] for r in resources}
        assert keys == {"org/repo1", "org/repo2"}
        assert all(r["resource_type"] == "repo" for r in resources)

    def test_list_resources_empty(self, center):
        """No bindings → empty list."""
        assert center.list_resources("alice", "github") == []

    def test_upsert_binding(self, db, center):
        """Binding same resource+name twice → updates, not duplicates."""
        center.bind_resource("alice", "github", "org/repo", {"read_token": "old"})
        center.bind_resource("alice", "github", "org/repo", {"read_token": "new"})

        assert center.get_resource_binding("alice", "github", "org/repo", "read_token") == "new"
        assert db.query(SkillResourceBinding).filter(
            SkillResourceBinding.resource_key == "org/repo",
            SkillResourceBinding.binding_name == "read_token",
        ).count() == 1

    def test_bindings_isolated_per_user(self, center):
        """Different users have independent bindings for the same resource."""
        center.bind_resource("alice", "github", "org/repo", {"read_token": "alice_tok"})
        center.bind_resource("bob", "github", "org/repo", {"read_token": "bob_tok"})

        assert center.get_resource_binding("alice", "github", "org/repo", "read_token") == "alice_tok"
        assert center.get_resource_binding("bob", "github", "org/repo", "read_token") == "bob_tok"


# ---------------------------------------------------------------------------
# 5. resolve_all — single call for execution
# ---------------------------------------------------------------------------


class TestResolveAll:
    """resolve_all returns complete SkillConfig for skill execution."""

    def test_resolve_with_resource(self, center):
        """Full resolution: settings + secrets + resource bindings."""
        center.set_setting("github", "api_base_url", "https://corp.github.com",
                           scope_type="tenant", scope_id="acme", updated_by="admin")
        center.set_setting("github", "default_token", "ghp_fallback",
                           scope_type="user", scope_id="alice", updated_by="alice")
        center.set_setting("github", "api_key", "key_123",
                           scope_type="user", scope_id="alice", updated_by="alice")
        center.bind_resource("alice", "github", "org/repo", {
            "read_token": "ghp_read",
            "write_token": "ghp_write",
        })

        config = center.resolve_all("github", "alice", tenant_id="acme", resource_key="org/repo")

        assert isinstance(config, SkillConfig)
        # Settings: tenant override + manifest default
        assert config.settings["api_base_url"] == "https://corp.github.com"
        assert config.settings["timeout"] == 30  # manifest default
        # Secrets
        assert config.secrets["default_token"] == "ghp_fallback"
        assert config.secrets["api_key"] == "key_123"
        # Resource
        assert config.resource is not None
        assert config.resource["read_token"] == "ghp_read"
        assert config.resource["write_token"] == "ghp_write"
        assert config.resource["default_branch"] == "main"  # manifest default
        assert config.resource_type == "repo"
        assert config.resource_key == "org/repo"

    def test_resolve_without_resource(self, center):
        """No resource_key → resource fields are None."""
        config = center.resolve_all("github", "alice")
        assert config.resource is None
        assert config.resource_type is None
        assert config.resource_key is None
        # Settings still resolved from manifest defaults
        assert config.settings["api_base_url"] == "https://api.github.com"

    def test_resolve_unknown_skill(self, center):
        """Unknown skill (no manifest) → empty config."""
        config = center.resolve_all("nonexistent", "alice")
        assert config.settings == {}
        assert config.secrets == {}
        assert config.resource is None
        assert config.resource_key is None

    def test_resolve_partial_resource_bindings(self, center):
        """Only some resource bindings set — missing ones use manifest default or absent."""
        center.set_setting("github", "api_key", "key_abc",
                           scope_type="user", scope_id="alice", updated_by="alice")
        # Only bind read_token, not write_token
        center.bind_resource("alice", "github", "org/repo", {"read_token": "ghp_read"})

        config = center.resolve_all("github", "alice", resource_key="org/repo")

        assert config.resource is not None
        assert config.resource["read_token"] == "ghp_read"
        assert "write_token" not in config.resource  # not set, no default
        assert config.resource["default_branch"] == "main"  # manifest default

    def test_resolve_resource_fallback_to_skill_secret(self, center):
        """Resource binding missing → falls back to skill-level setting."""
        center.set_setting("github", "read_token", "ghp_skill_level",
                           scope_type="user", scope_id="alice", updated_by="alice")

        config = center.resolve_all("github", "alice", resource_key="org/repo")

        assert config.resource is not None
        assert config.resource["read_token"] == "ghp_skill_level"

    def test_resolve_resource_key_none_when_no_manifest_resources(self, center):
        """resource_key passed but skill has no resources spec → resource_key is None."""
        # "nonexistent" skill has no manifest → no resources spec
        config = center.resolve_all("nonexistent", "alice", resource_key="org/repo")
        assert config.resource is None
        assert config.resource_key is None


# ---------------------------------------------------------------------------
# 6. Validation — required fields
# ---------------------------------------------------------------------------


class TestValidation:
    """validate() detects missing required config."""

    def test_missing_required_setting_and_secret(self, center):
        """Required setting/secret without default → error."""
        errors = center.validate("github", "alice")
        names = {e.name for e in errors}
        assert "instance_url" in names  # required setting, no default
        assert "api_key" in names       # required secret, no default

    def test_valid_after_config(self, center):
        """After setting required values → no errors (without resource)."""
        center.set_setting("github", "instance_url", "https://gh.corp.com",
                           scope_type="user", scope_id="alice", updated_by="alice")
        center.set_setting("github", "api_key", "key_abc",
                           scope_type="user", scope_id="alice", updated_by="alice")

        errors = center.validate("github", "alice")
        assert errors == []

    def test_missing_required_resource_binding(self, center):
        """Required resource binding missing → error with resource_key."""
        center.set_setting("github", "instance_url", "https://gh.corp.com",
                           scope_type="user", scope_id="alice", updated_by="alice")
        center.set_setting("github", "api_key", "key_abc",
                           scope_type="user", scope_id="alice", updated_by="alice")

        errors = center.validate("github", "alice", resource_key="org/repo")
        assert len(errors) == 1
        assert errors[0].section == "resources"
        assert errors[0].name == "read_token"
        assert errors[0].resource_key == "org/repo"
        assert "required" in errors[0].error

    def test_resource_binding_satisfied_by_fallback(self, center):
        """Required resource binding satisfied by skill-level secret fallback."""
        center.set_setting("github", "instance_url", "https://gh.corp.com",
                           scope_type="user", scope_id="alice", updated_by="alice")
        center.set_setting("github", "api_key", "key_abc",
                           scope_type="user", scope_id="alice", updated_by="alice")
        center.set_setting("github", "read_token", "ghp_fallback",
                           scope_type="user", scope_id="alice", updated_by="alice")

        errors = center.validate("github", "alice", resource_key="org/repo")
        assert errors == []

    def test_optional_fields_not_flagged(self, center):
        """Optional settings/secrets/bindings are not flagged as errors."""
        center.set_setting("github", "instance_url", "https://gh.corp.com",
                           scope_type="user", scope_id="alice", updated_by="alice")
        center.set_setting("github", "api_key", "key_abc",
                           scope_type="user", scope_id="alice", updated_by="alice")
        center.bind_resource("alice", "github", "org/repo", {"read_token": "tok"})

        # default_token (optional secret), write_token (optional binding) not set
        errors = center.validate("github", "alice", resource_key="org/repo")
        assert errors == []

    def test_unknown_skill_no_errors(self, center):
        """Unknown skill (no manifest) → no errors (nothing to validate)."""
        errors = center.validate("nonexistent", "alice")
        assert errors == []

    def test_error_dataclass_fields(self, center):
        """Verify ConfigValidationError has all expected fields."""
        errors = center.validate("github", "alice")
        for e in errors:
            assert isinstance(e, ConfigValidationError)
            assert e.section in ("settings", "secrets", "resources")
            assert isinstance(e.name, str)
            assert isinstance(e.error, str)
            assert len(e.error) > 0


# ---------------------------------------------------------------------------
# 7. Design capability: cross-user isolation
# ---------------------------------------------------------------------------


class TestCrossUserIsolation:
    """Settings and bindings are isolated per user — no cross-contamination."""

    def test_user_settings_isolated(self, center):
        """Alice's user-scope setting doesn't affect Bob."""
        center.set_setting("github", "timeout", "99",
                           scope_type="user", scope_id="alice", updated_by="alice")

        assert center.get_setting("github", "timeout", user_id="alice") == "99"
        assert center.get_setting("github", "timeout", user_id="bob") == 30  # manifest default

    def test_delete_one_user_doesnt_affect_other(self, db, center):
        """Deleting Alice's setting doesn't affect Bob's."""
        center.set_setting("github", "timeout", "10",
                           scope_type="user", scope_id="alice", updated_by="alice")
        center.set_setting("github", "timeout", "20",
                           scope_type="user", scope_id="bob", updated_by="bob")

        center.delete_setting("github", "timeout", scope_type="user", scope_id="alice")

        assert center.get_setting("github", "timeout", user_id="alice") == 30  # manifest default
        assert center.get_setting("github", "timeout", user_id="bob") == "20"
