"""Integration tests for Skill Configuration REST API (§13).

Tests all endpoints via HTTP with real DB:
- Settings CRUD with DB ground truth verification
- Secret encryption verification
- Resource binding lifecycle
- Validation
- Auth enforcement
- Global scope requires admin
"""

import pytest
from fastapi.testclient import TestClient

from api.main import app
from api.models.skill import SkillResourceBinding, SkillSetting
from api.routers import skill_config
from core.skills.config_center import SkillConfigCenter
from core.skills.credential_manager import CredentialManager

CRED_KEY = "test-key-for-api"

GITHUB_MANIFEST = {
    "name": "github",
    "version": "1.0.0",
    "settings": [
        {"name": "api_base_url", "type": "string", "default": "https://api.github.com"},
        {"name": "timeout", "type": "integer", "default": 30},
        {"name": "instance_url", "type": "url", "required": True},
    ],
    "secrets": [
        {"name": "default_token", "description": "Fallback token", "required": False},
        {"name": "api_key", "description": "Required API key", "required": True},
    ],
    "resources": {
        "type": "repo",
        "bindings": [
            {"name": "read_token", "type": "secret", "required": True},
            {"name": "write_token", "type": "secret", "required": False},
            {"name": "default_branch", "type": "string", "default": "main"},
        ],
    },
}


@pytest.fixture
def cred_mgr():
    return CredentialManager(CRED_KEY)


@pytest.fixture
def center(db_factory, cred_mgr):
    """Inject test SkillConfigCenter into the router."""
    c = SkillConfigCenter(
        db_factory, cred_mgr,
        manifest_loader=lambda name: GITHUB_MANIFEST if name == "github" else None,
    )
    skill_config._center = c
    yield c
    skill_config._center = None


@pytest.fixture
def client():
    return TestClient(app)


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
# Settings CRUD
# ---------------------------------------------------------------------------


class TestSettingsCRUD:
    def test_set_and_get_setting(self, client, auth_headers, test_user, center, db):
        resp = client.put(
            "/skills/github/config/instance_url",
            json={"value": "https://gh.corp.com"},
            headers=auth_headers,
        )
        assert resp.status_code == 200
        assert resp.json()["status"] == "ok"

        # Verify DB ground truth
        row = db.query(SkillSetting).filter_by(
            skill_name="github", setting_name="instance_url",
        ).one()
        assert row.setting_value == "https://gh.corp.com"
        assert row.scope_type == "user"
        assert row.scope_id == test_user.user_id
        assert row.updated_by == test_user.user_id
        assert row.is_secret == 0
        assert row.created_at is not None

        # GET effective config
        resp = client.get("/skills/github/config", headers=auth_headers)
        assert resp.status_code == 200
        data = resp.json()
        assert data["settings"]["instance_url"] == "https://gh.corp.com"
        assert data["settings"]["api_base_url"] == "https://api.github.com"

    def test_delete_setting(self, client, auth_headers, test_user, center, db):
        client.put(
            "/skills/github/config/instance_url",
            json={"value": "https://gh.corp.com"},
            headers=auth_headers,
        )
        resp = client.delete("/skills/github/config/instance_url", headers=auth_headers)
        assert resp.status_code == 200
        assert resp.json()["status"] == "deleted"

        # Verify DB ground truth — row gone
        assert db.query(SkillSetting).filter_by(
            skill_name="github", setting_name="instance_url",
        ).first() is None

    def test_delete_nonexistent_returns_404(self, client, auth_headers, center):
        resp = client.delete("/skills/github/config/no_such_setting", headers=auth_headers)
        assert resp.status_code == 404

    def test_secret_encrypted_in_db(self, client, auth_headers, test_user, center, db, cred_mgr):
        """Secret stored encrypted in DB, masked in API response."""
        client.put(
            "/skills/github/config/api_key",
            json={"value": "sk-secret-123"},
            headers=auth_headers,
        )

        # DB: encrypted
        row = db.query(SkillSetting).filter_by(
            skill_name="github", setting_name="api_key",
        ).one()
        assert row.is_secret == 1
        assert row.setting_value != "sk-secret-123"
        assert cred_mgr.decrypt(row.setting_value) == "sk-secret-123"

        # API: masked
        resp = client.get("/skills/github/config", headers=auth_headers)
        assert resp.json()["secrets"]["api_key"] == "***"

    def test_unknown_skill_returns_empty_config(self, client, auth_headers, center):
        resp = client.get("/skills/nonexistent/config", headers=auth_headers)
        assert resp.status_code == 200
        data = resp.json()
        assert data["settings"] == {}
        assert data["secrets"] == {}
        assert data["resources_configured"] == 0

    def test_rejects_invalid_value_type(self, client, auth_headers, center):
        """value must be scalar (str|int|float|bool), not list/dict/null."""
        resp = client.put(
            "/skills/github/config/timeout",
            json={"value": [1, 2, 3]},
            headers=auth_headers,
        )
        assert resp.status_code == 422

        resp = client.put(
            "/skills/github/config/timeout",
            json={"value": None},
            headers=auth_headers,
        )
        assert resp.status_code == 422


# ---------------------------------------------------------------------------
# Global scope — admin required
# ---------------------------------------------------------------------------


class TestGlobalScope:
    def test_regular_user_cannot_set_global(self, client, auth_headers, center):
        """Non-admin user gets 403 for global scope."""
        resp = client.put(
            "/skills/github/config/timeout?scope=global",
            json={"value": "60"},
            headers=auth_headers,
        )
        assert resp.status_code == 403

    def test_admin_can_set_global(self, client, admin_headers, center, db):
        """Admin user can write to global scope."""
        resp = client.put(
            "/skills/github/config/timeout?scope=global",
            json={"value": "60"},
            headers=admin_headers,
        )
        assert resp.status_code == 200

        row = db.query(SkillSetting).filter_by(
            skill_name="github", setting_name="timeout", scope_type="global",
        ).one()
        assert row.setting_value == "60"
        assert row.scope_id is None

    def test_invalid_scope_rejected(self, client, auth_headers, center):
        """Scope must be 'user' or 'global' — 'tenant' or garbage rejected."""
        resp = client.put(
            "/skills/github/config/timeout?scope=tenant",
            json={"value": "60"},
            headers=auth_headers,
        )
        assert resp.status_code == 422

        resp = client.put(
            "/skills/github/config/timeout?scope=invalid",
            json={"value": "60"},
            headers=auth_headers,
        )
        assert resp.status_code == 422


# ---------------------------------------------------------------------------
# Resource Bindings
# ---------------------------------------------------------------------------


class TestResourceBindings:
    def test_bind_list_unbind(self, client, auth_headers, center):
        # Bind
        resp = client.put(
            "/skills/github/resources/matrixorigin/matrixone",
            json={"bindings": {"read_token": "ghp_abc", "default_branch": "develop"}},
            headers=auth_headers,
        )
        assert resp.status_code == 200
        assert resp.json()["resource_key"] == "matrixorigin/matrixone"

        # List
        resp = client.get("/skills/github/resources", headers=auth_headers)
        assert resp.status_code == 200
        resources = resp.json()
        assert len(resources) == 1
        assert resources[0]["resource_key"] == "matrixorigin/matrixone"
        assert resources[0]["resource_type"] == "repo"

        # Unbind
        resp = client.delete(
            "/skills/github/resources/matrixorigin/matrixone",
            headers=auth_headers,
        )
        assert resp.status_code == 200
        assert resp.json()["count"] >= 1

        # List again — empty
        resp = client.get("/skills/github/resources", headers=auth_headers)
        assert resp.json() == []

    def test_unbind_nonexistent_returns_404(self, client, auth_headers, center):
        resp = client.delete(
            "/skills/github/resources/no/such/repo",
            headers=auth_headers,
        )
        assert resp.status_code == 404

    def test_binding_encrypted_in_db(self, client, auth_headers, test_user, center, db, cred_mgr):
        """Secret bindings are encrypted in DB."""
        client.put(
            "/skills/github/resources/org/repo",
            json={"bindings": {"read_token": "ghp_secret"}},
            headers=auth_headers,
        )

        row = db.query(SkillResourceBinding).filter_by(
            user_id=test_user.user_id, binding_name="read_token",
        ).one()
        assert row.is_secret == 1
        assert row.binding_value != "ghp_secret"
        assert cred_mgr.decrypt(row.binding_value) == "ghp_secret"


# ---------------------------------------------------------------------------
# Validation
# ---------------------------------------------------------------------------


class TestValidation:
    def test_missing_required_fields(self, client, auth_headers, center):
        resp = client.get("/skills/github/config/validate", headers=auth_headers)
        assert resp.status_code == 200
        data = resp.json()
        assert data["valid"] is False
        error_names = {e["name"] for e in data["errors"]}
        assert "instance_url" in error_names
        assert "api_key" in error_names

    def test_valid_after_setting_required(self, client, auth_headers, center):
        client.put("/skills/github/config/instance_url",
                   json={"value": "https://gh.corp.com"}, headers=auth_headers)
        client.put("/skills/github/config/api_key",
                   json={"value": "sk-123"}, headers=auth_headers)

        resp = client.get("/skills/github/config/validate", headers=auth_headers)
        data = resp.json()
        assert data["valid"] is True
        assert data["errors"] == []

    def test_validate_with_resource(self, client, auth_headers, center):
        """Set required settings first, then validate resource-level config."""
        client.put("/skills/github/config/instance_url",
                   json={"value": "https://gh.corp.com"}, headers=auth_headers)
        client.put("/skills/github/config/api_key",
                   json={"value": "sk-123"}, headers=auth_headers)

        # Resource binding missing → only resource-level error
        resp = client.get(
            "/skills/github/config/validate",
            params={"resource": "matrixorigin/matrixone"},
            headers=auth_headers,
        )
        data = resp.json()
        assert data["valid"] is False
        assert len(data["errors"]) == 1
        assert data["errors"][0]["section"] == "resources"
        assert data["errors"][0]["name"] == "read_token"
        assert data["errors"][0]["resource_key"] == "matrixorigin/matrixone"

    def test_validate_route_not_shadowed_by_setting_name(self, client, auth_headers, center):
        """GET /config/validate must not be matched as DELETE /config/{setting_name}."""
        resp = client.get("/skills/github/config/validate", headers=auth_headers)
        assert resp.status_code == 200
        assert "valid" in resp.json()


# ---------------------------------------------------------------------------
# Auth required
# ---------------------------------------------------------------------------


class TestAuth:
    def test_no_auth_returns_401(self, client, center):
        resp = client.get("/skills/github/config")
        assert resp.status_code in (401, 403)

    def test_no_auth_on_put(self, client, center):
        resp = client.put("/skills/github/config/foo", json={"value": "bar"})
        assert resp.status_code in (401, 403)
