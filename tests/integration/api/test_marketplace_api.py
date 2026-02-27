"""Integration tests for marketplace API (install/uninstall/upgrade + credentials)."""

import pytest
from fastapi.testclient import TestClient
from uuid import uuid4

from api.main import app
from api.database import get_db_session


# ── fixtures ──────────────────────────────────────────────────────────────────

@pytest.fixture
def client():
    return TestClient(app)


@pytest.fixture
def db_session():
    session = next(get_db_session())
    yield session
    session.close()


# auth_headers fixture now provided by tests/integration/conftest.py


@pytest.fixture(autouse=True)
def seed_skill_definition(db_session, test_user):
    """Insert a public skill in skill_registry so install works."""
    from api.models import SkillRegistry, SkillInstallation, UserCredential

    # Clean up
    db_session.query(SkillInstallation).filter(SkillInstallation.skill_name == "github").delete()
    db_session.query(UserCredential).filter(UserCredential.skill_name == "github").delete()
    db_session.query(SkillRegistry).filter(SkillRegistry.skill_name == "github").delete()

    db_session.add(SkillRegistry(
        skill_id=str(uuid4()),
        skill_name="github",
        version="1.0.0",
        description="GitHub integration",
        manifest={},
        is_active=True,
        is_public=True,
        source="marketplace",
        created_by=test_user.user_id,
    ))
    db_session.commit()
    yield
    db_session.query(SkillInstallation).filter(SkillInstallation.skill_name == "github").delete()
    db_session.query(UserCredential).filter(UserCredential.skill_name == "github").delete()
    db_session.query(SkillRegistry).filter(SkillRegistry.skill_name == "github").delete()
    db_session.commit()


# ── install / uninstall / upgrade ─────────────────────────────────────────────

def test_install_skill(client, auth_headers):
    resp = client.post("/marketplace/install", headers=auth_headers, json={"skill_name": "github"})
    assert resp.status_code == 201
    data = resp.json()
    assert data["skill_name"] == "github"
    assert data["skill_version"] == "1.0.0"
    assert data["status"] == "installed"


def test_install_idempotent(client, auth_headers):
    client.post("/marketplace/install", headers=auth_headers, json={"skill_name": "github"})
    resp = client.post("/marketplace/install", headers=auth_headers, json={"skill_name": "github"})
    assert resp.status_code == 201


def test_install_not_found(client, auth_headers):
    resp = client.post("/marketplace/install", headers=auth_headers, json={"skill_name": "nonexistent"})
    assert resp.status_code == 404


def test_list_installed(client, auth_headers):
    client.post("/marketplace/install", headers=auth_headers, json={"skill_name": "github"})
    resp = client.get("/marketplace/installed", headers=auth_headers)
    assert resp.status_code == 200
    names = [i["skill_name"] for i in resp.json()["installations"]]
    assert "github" in names


def test_uninstall_skill(client, auth_headers):
    client.post("/marketplace/install", headers=auth_headers, json={"skill_name": "github"})
    resp = client.post("/marketplace/uninstall", headers=auth_headers, json={"skill_name": "github"})
    assert resp.status_code == 204

    # Should no longer appear in installed list
    resp = client.get("/marketplace/installed", headers=auth_headers)
    names = [i["skill_name"] for i in resp.json()["installations"]]
    assert "github" not in names


def test_uninstall_not_installed(client, auth_headers):
    resp = client.post("/marketplace/uninstall", headers=auth_headers, json={"skill_name": "github"})
    assert resp.status_code == 404


def test_upgrade_skill(client, auth_headers, db_session):
    client.post("/marketplace/install", headers=auth_headers, json={"skill_name": "github"})

    # Bump definition version
    from api.models import SkillRegistry
    db_session.query(SkillRegistry).filter(SkillRegistry.skill_name == "github").update({"version": "2.0.0"})
    db_session.commit()

    resp = client.post("/marketplace/upgrade", headers=auth_headers, json={"skill_name": "github"})
    assert resp.status_code == 200
    assert resp.json()["skill_version"] == "2.0.0"


def test_upgrade_not_installed(client, auth_headers):
    resp = client.post("/marketplace/upgrade", headers=auth_headers, json={"skill_name": "github"})
    assert resp.status_code == 404


# ── credentials ───────────────────────────────────────────────────────────────

def test_save_and_delete_credential(client, auth_headers):
    # Save
    resp = client.post("/marketplace/credentials", headers=auth_headers, json={
        "skill_name": "github", "credential_name": "token", "value": "ghp_abc123",
    })
    assert resp.status_code == 204

    # Delete
    resp = client.delete(
        "/marketplace/credentials",
        headers=auth_headers,
        params={"skill_name": "github", "credential_name": "token"},
    )
    assert resp.status_code == 204


def test_delete_credential_not_found(client, auth_headers):
    resp = client.delete(
        "/marketplace/credentials",
        headers=auth_headers,
        params={"skill_name": "github", "credential_name": "nonexistent"},
    )
    assert resp.status_code == 404


def test_uninstall_deletes_credentials(client, auth_headers):
    """Uninstall should also delete all credentials for that skill."""
    client.post("/marketplace/install", headers=auth_headers, json={"skill_name": "github"})
    client.post("/marketplace/credentials", headers=auth_headers, json={
        "skill_name": "github", "credential_name": "token", "value": "ghp_secret",
    })
    client.post("/marketplace/uninstall", headers=auth_headers, json={"skill_name": "github"})

    # Credential should be gone — deleting again should 404
    resp = client.delete(
        "/marketplace/credentials",
        headers=auth_headers,
        params={"skill_name": "github", "credential_name": "token"},
    )
    assert resp.status_code == 404


def test_credential_update_overwrites(client, auth_headers):
    """Saving the same credential twice should overwrite, not error."""
    client.post("/marketplace/credentials", headers=auth_headers, json={
        "skill_name": "github", "credential_name": "token", "value": "v1",
    })
    resp = client.post("/marketplace/credentials", headers=auth_headers, json={
        "skill_name": "github", "credential_name": "token", "value": "v2",
    })
    assert resp.status_code == 204


def test_install_permission_denied(client, auth_headers, db_session):
    """Non-public skill without explicit permission should 403."""
    from api.models import SkillRegistry

    db_session.add(SkillRegistry(
        skill_id=str(uuid4()),
        skill_name="private_skill",
        version="1.0.0",
        description="Private",
        manifest={},
        is_active=True,
        is_public=False,
        source="marketplace",
        created_by="admin",
    ))
    db_session.commit()

    resp = client.post("/marketplace/install", headers=auth_headers, json={"skill_name": "private_skill"})
    assert resp.status_code == 403

    # Cleanup
    db_session.query(SkillRegistry).filter(SkillRegistry.skill_name == "private_skill").delete()
    db_session.commit()
