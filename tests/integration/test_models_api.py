"""Integration tests for models API."""

import pytest
from fastapi.testclient import TestClient

from api.main import app


@pytest.fixture
def client():
    """Create test client."""
    return TestClient(app)


def test_create_and_list_models(client, auth_headers):
    """Test model creation and listing."""
    # Create model
    response = client.post(
        "/models",
        json={
            "name": "gpt-4o",
            "provider": "openai",
            "scope": "global",
        },
        headers=auth_headers,
    )
    assert response.status_code == 201
    data = response.json()
    assert data["name"] == "gpt-4o"
    assert data["provider"] == "openai"
    assert data["scope"] == "global"

    # List models
    response = client.get("/models", headers=auth_headers)
    assert response.status_code == 200
    models = response.json()
    assert isinstance(models, list)


def test_create_model_with_scope_id(client, auth_headers):
    """Test model creation with scope_id."""
    response = client.post(
        "/models",
        json={
            "name": "claude-3",
            "provider": "anthropic",
            "scope": "account",
            "scope_id": "acme",
        },
        headers=auth_headers,
    )
    assert response.status_code == 201
    data = response.json()
    assert data["name"] == "claude-3"
    assert data["provider"] == "anthropic"
    assert data["scope"] == "account"
    assert data["scope_id"] == "acme"


def test_models_require_auth(client):
    """Test that models endpoints require authentication."""
    # Create without auth
    response = client.post(
        "/models",
        json={"name": "gpt-4", "provider": "openai"},
    )
    assert response.status_code == 401

    # List without auth
    response = client.get("/models")
    assert response.status_code == 401
