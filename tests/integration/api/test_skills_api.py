"""Integration tests for skills API."""

import pytest
from fastapi.testclient import TestClient
from uuid import uuid4

from api.main import app
from api.database import get_db_session


@pytest.fixture(autouse=True)
def cleanup_skills():
    """Clean up test skills before and after each test."""
    from sqlalchemy.orm import Session
    from sqlalchemy import text
    from api.routers.skills import reset_catalog

    db = next(get_db_session())

    # Clean before
    reset_catalog()
    db.execute(
        text('DELETE FROM skills_registry WHERE skill_name LIKE "Test%" OR skill_name LIKE "Get%"')
    )
    db.commit()

    yield

    # Clean after
    reset_catalog()
    db.execute(
        text('DELETE FROM skills_registry WHERE skill_name LIKE "Test%" OR skill_name LIKE "Get%"')
    )
    db.commit()
    db.close()


@pytest.fixture
def client():
    return TestClient(app)


@pytest.fixture
def db_session():
    session = next(get_db_session())
    yield session
    session.close()


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
