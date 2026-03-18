"""Integration tests for skills API."""

import pytest
from fastapi.testclient import TestClient
from uuid import uuid4

from api.main import app
from api.database import get_db_session


@pytest.fixture(autouse=True)
def cleanup_skills(db_session):
    """Clean up test skills before and after each test."""
    from sqlalchemy import text
    from api.routers.skills import reset_catalog

    # Clean before
    reset_catalog()
    db_session.execute(
        text('DELETE FROM skills_registry WHERE skill_name LIKE "Test%" OR skill_name LIKE "Get%"')
    )
    db_session.commit()

    yield

    # Clean after
    reset_catalog()
    db_session.execute(
        text('DELETE FROM skills_registry WHERE skill_name LIKE "Test%" OR skill_name LIKE "Get%"')
    )
    db_session.commit()


@pytest.fixture
def client(db_session):
    def override_get_db():
        try:
            yield db_session
        finally:
            pass

    app.dependency_overrides[get_db_session] = override_get_db
    try:
        yield TestClient(app)
    finally:
        app.dependency_overrides.pop(get_db_session, None)


# auth_headers fixture now provided by tests/integration/conftest.py


def test_register_skill_success(client, auth_headers):
    """Test successful skill registration."""
    skill_id = f"test_skill_{uuid4().hex}"

    response = client.post(
        "/skills",
        headers=auth_headers,
        json={
            "skill_id": skill_id,
            "skill_name": "Test Skill",
            "skill_version": "1.0.0",
            "skill_code": "def test(): pass",
            "description": "A test skill",
            "metadata": {"category": "test"},
        },
    )

    if response.status_code != 201:
        print(f"Error response: {response.text}")

    assert response.status_code == 201
    data = response.json()
    assert data["skill_id"] == skill_id
    assert data["skill_name"] == "Test Skill"
    assert data["version"] == "1.0.0"


def test_get_skill_success(client, auth_headers):
    """Test successful skill retrieval."""
    skill_id = f"test_skill_{uuid4().hex}"

    # Register first
    client.post(
        "/skills",
        headers=auth_headers,
        json={
            "skill_id": skill_id,
            "skill_name": "Get Test",
            "skill_version": "1.0.0",
            "skill_code": "pass",
        },
    )

    # Get skill
    response = client.get(f"/skills/{skill_id}", headers=auth_headers)

    assert response.status_code == 200
    data = response.json()
    assert data["skill_id"] == skill_id


def test_list_skills_success(client, auth_headers):
    """Test successful skill listing."""
    response = client.get("/skills", headers=auth_headers)

    assert response.status_code == 200
    data = response.json()
    assert "skills" in data
    assert "total" in data


def test_get_skill_not_found(client, auth_headers):
    """Test get non-existent skill."""
    response = client.get("/skills/nonexistent", headers=auth_headers)
    assert response.status_code == 404
