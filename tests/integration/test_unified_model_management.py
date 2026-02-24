"""Integration tests for unified model management system."""

import json

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
def clean_model_registry(db_session):
    db_session.execute(text("DELETE FROM configs WHERE key_name = 'model_registry'"))
    db_session.commit()
    yield
    db_session.execute(text("DELETE FROM configs WHERE key_name = 'model_registry'"))
    db_session.commit()


def test_create_model_with_rich_metadata(client, admin_headers, clean_model_registry):
    """Test creating a model with full metadata."""
    response = client.post(
        "/models",
        headers=admin_headers,
        json={
            "name": "gpt-4o",
            "provider": "openai",
            "scope": "global",
            "context_window": 128000,
            "max_completion_tokens": 16384,
            "input_modalities": ["text", "image"],
            "output_modalities": ["text"],
            "supported_parameters": ["tools", "vision"],
            "pricing": {"prompt": 0.0025, "completion": 0.01},
            "architecture": "transformer",
            "tags": ["code", "reasoning"],
        },
    )
    assert response.status_code == 201
    data = response.json()
    assert data["name"] == "gpt-4o"
    assert data["input_modalities"] == ["text", "image"]
    assert data["pricing"]["prompt"] == 0.0025
    assert data["architecture"] == "transformer"


def test_create_duplicate_model_fails(client, admin_headers, clean_model_registry):
    client.post("/models", headers=admin_headers, json={"name": "gpt-4o", "provider": "openai"})
    response = client.post("/models", headers=admin_headers, json={"name": "gpt-4o", "provider": "openai"})
    assert response.status_code == 400
    assert "already exists" in response.json()["detail"]


def test_list_models_returns_rich_metadata(client, admin_headers, clean_model_registry):
    client.post(
        "/models",
        headers=admin_headers,
        json={
            "name": "gpt-4o",
            "provider": "openai",
            "pricing": {"prompt": 0.0025, "completion": 0.01},
            "supported_parameters": ["tools"],
        },
    )
    response = client.get("/models", headers=admin_headers)
    assert response.status_code == 200
    models = response.json()
    assert len(models) == 1
    assert models[0]["pricing"]["prompt"] == 0.0025
    assert models[0]["supported_parameters"] == ["tools"]


def test_list_models_auto_seeds_when_empty(client, admin_headers, clean_model_registry):
    """When no models exist, auto-seed provides defaults."""
    response = client.get("/models", headers=admin_headers)
    assert response.status_code == 200
    models = response.json()
    assert len(models) > 0  # Auto-seeded
    assert any(m["name"] == "gpt-4o" for m in models)


def test_delete_model(client, admin_headers, clean_model_registry):
    client.post("/models", headers=admin_headers, json={"name": "test-delete-me", "provider": "openai"})
    response = client.delete("/models/test-delete-me?scope=global", headers=admin_headers)
    assert response.status_code == 204
    models = client.get("/models", headers=admin_headers).json()
    assert not any(m["name"] == "test-delete-me" for m in models)


def test_delete_nonexistent_model_fails(client, admin_headers, clean_model_registry):
    response = client.delete("/models/nonexistent?scope=global", headers=admin_headers)
    assert response.status_code == 404


def test_model_registry_persists_across_llm_client_instances(db_session, clean_model_registry):
    from core.llm.client import LLMClient

    models = [
        {
            "model_name": "gpt-4o",
            "provider": "openai",
            "context_window": 128000,
            "pricing": {"prompt": 0.0025, "completion": 0.01},
            "supported_parameters": ["tools", "vision"],
            "tags": ["code"],
            "is_active": True,
        }
    ]
    db_session.execute(
        text("INSERT INTO configs (config_id, key_name, value, scope_type) VALUES (:id, 'model_registry', :value, 'global')"),
        {"id": "test-id", "value": json.dumps(models)},
    )
    db_session.commit()

    llm = LLMClient(db=db_session, user_id="test-user")
    active = llm.router.registry.list_active()
    assert len(active) == 1
    assert active[0].model_name == "gpt-4o"
    assert active[0].pricing.prompt == 0.0025
    assert active[0].supported_parameters == ["tools", "vision"]


def test_backward_compat_flat_pricing_fields(db_session, clean_model_registry):
    """Old DB records with flat price_per_1k_* fields still load correctly."""
    from core.llm.client import LLMClient

    models = [
        {
            "model_name": "old-model",
            "provider": "openai",
            "price_per_1k_prompt": 0.03,
            "price_per_1k_completion": 0.06,
            "is_active": True,
        }
    ]
    db_session.execute(
        text("INSERT INTO configs (config_id, key_name, value, scope_type) VALUES (:id, 'model_registry', :value, 'global')"),
        {"id": "compat-id", "value": json.dumps(models)},
    )
    db_session.commit()

    llm = LLMClient(db=db_session)
    m = llm.router.registry.get("old-model")
    assert m is not None
    assert m.pricing.prompt == 0.03
    assert m.pricing.completion == 0.06


def test_user_scope_overrides_global_scope(db_session, clean_model_registry):
    from core.llm.client import LLMClient

    db_session.execute(
        text("INSERT INTO configs (config_id, key_name, value, scope_type) VALUES ('g-id', 'model_registry', :v, 'global')"),
        {"v": json.dumps([{"model_name": "gpt-4o", "provider": "openai", "pricing": {"prompt": 0.0025, "completion": 0.01}, "is_active": True}])},
    )
    db_session.execute(
        text("INSERT INTO configs (config_id, key_name, value, scope_type, scope_user_id) VALUES ('u-id', 'model_registry', :v, 'user', 'alice')"),
        {"v": json.dumps([{"model_name": "gpt-4o", "provider": "openai", "pricing": {"prompt": 0.001, "completion": 0.005}, "is_active": True}])},
    )
    db_session.commit()

    llm = LLMClient(db=db_session, user_id="alice")
    models = llm.router.registry.list_active()
    assert len(models) == 1
    assert models[0].pricing.prompt == 0.001


def test_update_model(client, admin_headers, clean_model_registry):
    client.post("/models", headers=admin_headers, json={"name": "gpt-4o", "provider": "openai", "tags": ["code"]})
    response = client.put(
        "/models/gpt-4o?scope=global",
        headers=admin_headers,
        json={"tags": ["code", "reasoning"], "pricing": {"prompt": 0.005, "completion": 0.02}},
    )
    assert response.status_code == 200
    assert response.json()["tags"] == ["code", "reasoning"]
    assert response.json()["pricing"]["prompt"] == 0.005


def test_update_model_persists(client, admin_headers, clean_model_registry, db_session):
    from core.llm.client import LLMClient

    client.post("/models", headers=admin_headers, json={"name": "gpt-4o", "provider": "openai"})
    client.put("/models/gpt-4o?scope=global", headers=admin_headers, json={"tags": ["code", "reasoning"]})

    llm = LLMClient(db=db_session)
    model = llm.router.registry.get("gpt-4o")
    assert model is not None
    assert "reasoning" in model.tags


def test_update_nonexistent_model_fails(client, admin_headers, clean_model_registry):
    response = client.put("/models/nonexistent?scope=global", headers=admin_headers, json={"tags": ["test"]})
    assert response.status_code == 404


def test_delete_model_and_verify_gone(client, admin_headers, clean_model_registry, db_session):
    from core.llm.client import LLMClient

    client.post("/models", headers=admin_headers, json={"name": "temp-model", "provider": "mock"})
    client.delete("/models/temp-model?scope=global", headers=admin_headers)

    llm = LLMClient(db=db_session)
    assert llm.router.registry.get("temp-model") is None
