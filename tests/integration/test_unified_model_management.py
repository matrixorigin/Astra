"""Integration tests for unified model management (llm_models table)."""

import pytest
from fastapi.testclient import TestClient
from sqlalchemy import text

from api.database import get_db_session
from api.main import app


@pytest.fixture
def client(db_session):
    def override_get_db():
        try:
            yield db_session
        finally:
            pass

    app.dependency_overrides[get_db_session] = override_get_db
    yield TestClient(app)
    app.dependency_overrides.clear()


@pytest.fixture
def clean_models(db_session):
    db_session.execute(text("DELETE FROM llm_models"))
    db_session.commit()
    yield
    db_session.execute(text("DELETE FROM llm_models"))
    db_session.commit()


def _create_model(client, admin_headers, **overrides):
    payload = {"name": "test-model", "provider": "mock", "api_key": "sk-test-key", **overrides}
    return client.post("/models", headers=admin_headers, json=payload)


# --- Create ---

def test_create_model_with_rich_metadata(client, admin_headers, clean_models):
    response = _create_model(
        client, admin_headers,
        name="gpt-4o", provider="openai", api_key="sk-test-123",
        context_window=128000, max_completion_tokens=16384,
        input_modalities=["text", "image"], output_modalities=["text"],
        supported_parameters=["tools", "vision"],
        pricing={"prompt": 0.0025, "completion": 0.01},
        architecture="transformer", tags=["code", "reasoning"],
    )
    assert response.status_code == 201
    data = response.json()
    assert data["name"] == "gpt-4o"
    assert data["input_modalities"] == ["text", "image"]
    assert data["pricing"]["prompt"] == 0.0025
    assert data["architecture"] == "transformer"


def test_create_model_requires_api_key(client, admin_headers, clean_models):
    response = client.post("/models", headers=admin_headers, json={"name": "m", "provider": "mock"})
    assert response.status_code == 422


def test_create_duplicate_model_fails(client, admin_headers, clean_models):
    _create_model(client, admin_headers, name="dup-model")
    response = _create_model(client, admin_headers, name="dup-model")
    assert response.status_code == 400
    assert "already exists" in response.json()["detail"]


# --- List ---

def test_list_models_returns_rich_metadata(client, admin_headers, clean_models):
    _create_model(
        client, admin_headers, name="gpt-4o",
        pricing={"prompt": 0.0025, "completion": 0.01},
        supported_parameters=["tools"],
    )
    response = client.get("/models", headers=admin_headers)
    assert response.status_code == 200
    models = response.json()
    assert len(models) >= 1
    m = next(m for m in models if m["name"] == "gpt-4o")
    assert m["pricing"]["prompt"] == 0.0025
    assert m["supported_parameters"] == ["tools"]


def test_list_models_empty(client, admin_headers, clean_models):
    response = client.get("/models", headers=admin_headers)
    assert response.status_code == 200
    assert response.json() == []


# --- Update ---

def test_update_model(client, admin_headers, clean_models):
    _create_model(client, admin_headers, name="gpt-4o", tags=["code"])
    response = client.put(
        "/models/gpt-4o", headers=admin_headers,
        json={"tags": ["code", "reasoning"]},
    )
    assert response.status_code == 200
    assert response.json()["tags"] == ["code", "reasoning"]


def test_update_nonexistent_model_fails(client, admin_headers, clean_models):
    response = client.put("/models/nonexistent", headers=admin_headers, json={"tags": ["test"]})
    assert response.status_code == 404


# --- Delete ---

def test_delete_model(client, admin_headers, clean_models):
    _create_model(client, admin_headers, name="to-delete")
    response = client.delete("/models/to-delete", headers=admin_headers)
    assert response.status_code == 204
    models = client.get("/models", headers=admin_headers).json()
    assert not any(m["name"] == "to-delete" for m in models)


def test_delete_nonexistent_model_fails(client, admin_headers, clean_models):
    response = client.delete("/models/nonexistent", headers=admin_headers)
    assert response.status_code == 404


# --- Connectivity ---

def test_create_model_validates_connectivity(client, admin_headers, clean_models):
    """Model with bad key should be created but inactive."""
    response = _create_model(
        client, admin_headers, name="bad-key-model",
        provider="openai", api_key="sk-invalid-key",
    )
    assert response.status_code == 201
    data = response.json()
    assert data["is_active"] is False  # connectivity failed


def test_connectivity_uses_user_provided_model_name(client, admin_headers, clean_models, monkeypatch):
    """Verify connectivity validation uses the model name provided by user, not a default."""
    captured_model_names = []

    original_validate = None
    import api.routers.models as models_module
    original_validate = models_module._validate_connectivity

    def mock_validate(provider, model_name, api_key, base_url):
        captured_model_names.append(model_name)
        return None  # success

    monkeypatch.setattr(models_module, "_validate_connectivity", mock_validate)

    # Register a custom model name that differs from any default
    response = _create_model(
        client, admin_headers,
        name="my-custom-gpt-4o-variant",
        provider="openai",
        api_key="sk-test",
    )
    assert response.status_code == 201
    assert "my-custom-gpt-4o-variant" in captured_model_names


# --- API key not exposed ---

def test_api_key_not_in_response(client, admin_headers, clean_models):
    response = _create_model(client, admin_headers, name="secret-model")
    assert response.status_code == 201
    data = response.json()
    assert "api_key" not in data
    assert "api_key_encrypted" not in data
