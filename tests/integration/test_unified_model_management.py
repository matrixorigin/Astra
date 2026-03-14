"""Integration tests for unified model management (infra_llm_models table)."""

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
    db_session.execute(text("DELETE FROM infra_llm_models"))
    db_session.commit()
    yield
    db_session.execute(text("DELETE FROM infra_llm_models"))
    db_session.commit()


def _create_model(client, admin_headers, **overrides):
    payload = {"name": "test-model", "provider": "mock", "api_key": "sk-test-key", **overrides}
    return client.post("/models", headers=admin_headers, json=payload)


# --- Create ---


def test_create_model_with_rich_metadata(client, admin_headers, clean_models):
    response = _create_model(
        client,
        admin_headers,
        name="gpt-4o",
        provider="openai",
        api_key="sk-test-123",
        context_window=128000,
        max_completion_tokens=16384,
        input_modalities=["text", "image"],
        output_modalities=["text"],
        supported_parameters=["tools", "vision"],
        pricing={"prompt": 0.0025, "completion": 0.01},
        architecture="transformer",
        tags=["code", "reasoning"],
    )
    assert response.status_code == 201
    data = response.json()
    assert data["name"] == "gpt-4o"
    assert data["input_modalities"] == ["text", "image"]
    assert data["pricing"]["prompt"] == 0.0025
    assert data["architecture"] == "transformer"


def test_create_model_with_description(client, admin_headers, clean_models):
    response = _create_model(
        client,
        admin_headers,
        name="ds-private",
        provider="mock",
        description="MagikCloud private DeepSeek V3",
    )
    assert response.status_code == 201
    data = response.json()
    assert data["description"] == "MagikCloud private DeepSeek V3"


def test_create_model_description_none_by_default(client, admin_headers, clean_models):
    response = _create_model(client, admin_headers, name="no-desc")
    assert response.status_code == 201
    assert response.json()["description"] is None


def test_create_model_requires_api_key(client, admin_headers, clean_models):
    response = client.post("/models", headers=admin_headers, json={"name": "m", "provider": "mock"})
    assert response.status_code == 422


def test_create_duplicate_model_fails(client, admin_headers, clean_models):
    _create_model(client, admin_headers, name="dup-model")
    response = _create_model(client, admin_headers, name="dup-model")
    assert response.status_code == 400
    assert "already exists" in response.json()["detail"]


def test_create_model_non_admin_forbidden(client, clean_models, test_user):
    """Non-admin user gets 403."""
    # Login as test_user (not admin)
    login = client.post(
        "/auth/login", json={"username": test_user.username, "password": "testpass123"}
    )
    token = login.json()["access_token"]
    headers = {"Authorization": f"Bearer {token}"}
    # Remove admin role
    from api.database import SessionLocal

    db = SessionLocal()
    try:
        db.execute(
            text("DELETE FROM auth_user_roles WHERE user_id = :uid"), {"uid": test_user.user_id}
        )
        db.commit()
    finally:
        db.close()
    response = client.post(
        "/models", headers=headers, json={"name": "m", "provider": "mock", "api_key": "k"}
    )
    assert response.status_code == 403


# --- List ---


def test_list_models_returns_rich_metadata(client, admin_headers, clean_models):
    _create_model(
        client,
        admin_headers,
        name="gpt-4o",
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


def test_list_models_shows_description(client, admin_headers, clean_models):
    _create_model(client, admin_headers, name="m1", description="First model")
    _create_model(
        client, admin_headers, name="m2", provider="mock", api_key="k2", description="Second model"
    )
    models = client.get("/models", headers=admin_headers).json()
    descs = {m["name"]: m["description"] for m in models}
    assert descs["m1"] == "First model"
    assert descs["m2"] == "Second model"


def test_list_models_non_admin_hides_inactive(client, admin_headers, clean_models, test_user):
    """Non-admin users only see active models."""
    _create_model(client, admin_headers, name="active-m")
    _create_model(client, admin_headers, name="inactive-m")
    # Deactivate one
    client.put("/models/inactive-m", headers=admin_headers, json={"is_active": False})
    # Login as non-admin
    from api.database import SessionLocal

    db = SessionLocal()
    try:
        db.execute(
            text("DELETE FROM auth_user_roles WHERE user_id = :uid"), {"uid": test_user.user_id}
        )
        db.commit()
    finally:
        db.close()
    login = client.post(
        "/auth/login", json={"username": test_user.username, "password": "testpass123"}
    )
    headers = {"Authorization": f"Bearer {login.json()['access_token']}"}
    models = client.get("/models", headers=headers).json()
    names = [m["name"] for m in models]
    assert "active-m" in names
    assert "inactive-m" not in names


# --- Get single model ---


def test_get_model(client, admin_headers, clean_models):
    _create_model(client, admin_headers, name="get-me", description="test desc", tags=["fast"])
    response = client.get("/models/get-me", headers=admin_headers)
    assert response.status_code == 200
    data = response.json()
    assert data["name"] == "get-me"
    assert data["description"] == "test desc"
    assert data["tags"] == ["fast"]


def test_get_model_not_found(client, admin_headers, clean_models):
    response = client.get("/models/nonexistent", headers=admin_headers)
    assert response.status_code == 404


# --- Update ---


def test_update_model(client, admin_headers, clean_models):
    _create_model(client, admin_headers, name="gpt-4o", tags=["code"])
    response = client.put(
        "/models/gpt-4o",
        headers=admin_headers,
        json={"tags": ["code", "reasoning"]},
    )
    assert response.status_code == 200
    assert response.json()["tags"] == ["code", "reasoning"]


def test_update_model_description(client, admin_headers, clean_models):
    _create_model(client, admin_headers, name="upd-desc")
    response = client.put(
        "/models/upd-desc",
        headers=admin_headers,
        json={"description": "Updated description"},
    )
    assert response.status_code == 200
    assert response.json()["description"] == "Updated description"


def test_update_model_all_fields(client, admin_headers, clean_models):
    """Update every mutable field to cover all branches."""
    _create_model(client, admin_headers, name="full-upd")
    response = client.put(
        "/models/full-upd",
        headers=admin_headers,
        json={
            "base_url": "https://custom.api/v1",
            "description": "new desc",
            "context_window": 64000,
            "max_completion_tokens": 4096,
            "input_modalities": ["text", "image"],
            "output_modalities": ["text", "audio"],
            "supported_parameters": ["tools", "vision"],
            "pricing": {"prompt": 0.01, "completion": 0.02},
            "architecture": "moe",
            "tags": ["cheap", "fast"],
            "is_active": False,
        },
    )
    assert response.status_code == 200
    data = response.json()
    assert data["base_url"] == "https://custom.api/v1"
    assert data["description"] == "new desc"
    assert data["context_window"] == 64000
    assert data["max_completion_tokens"] == 4096
    assert data["input_modalities"] == ["text", "image"]
    assert data["output_modalities"] == ["text", "audio"]
    assert data["supported_parameters"] == ["tools", "vision"]
    assert data["pricing"]["prompt"] == 0.01
    assert data["architecture"] == "moe"
    assert data["tags"] == ["cheap", "fast"]
    assert data["is_active"] is False


def test_update_model_api_key_revalidates(client, admin_headers, clean_models, monkeypatch):
    """Updating API key triggers connectivity re-validation."""
    _create_model(client, admin_headers, name="rekey")
    import api.routers.models as models_module

    monkeypatch.setattr(models_module, "_validate_connectivity", lambda *a: None)
    response = client.put(
        "/models/rekey",
        headers=admin_headers,
        json={"api_key": "sk-new-key"},
    )
    assert response.status_code == 200
    assert response.json()["connectivity"] == "ok"
    assert response.json()["is_active"] is True


def test_update_model_api_key_fail_deactivates(client, admin_headers, clean_models, monkeypatch):
    """Failed connectivity on key update deactivates model."""
    _create_model(client, admin_headers, name="badkey")
    import api.routers.models as models_module

    monkeypatch.setattr(
        models_module, "_validate_connectivity", lambda *a: "HTTP 401: Unauthorized"
    )
    response = client.put(
        "/models/badkey",
        headers=admin_headers,
        json={"api_key": "sk-bad"},
    )
    assert response.status_code == 200
    assert response.json()["is_active"] is False
    assert "401" in response.json()["connectivity"]


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


# --- Check connectivity ---


def test_check_model_ok(client, admin_headers, clean_models, monkeypatch):
    _create_model(client, admin_headers, name="check-ok")
    import api.routers.models as models_module

    monkeypatch.setattr(models_module, "_validate_connectivity", lambda *a: None)
    response = client.post("/models/check-ok/check", headers=admin_headers)
    assert response.status_code == 200
    assert response.json()["connectivity"] == "ok"
    assert response.json()["is_active"] is True


def test_check_model_fail(client, admin_headers, clean_models, monkeypatch):
    _create_model(client, admin_headers, name="check-fail")
    import api.routers.models as models_module

    monkeypatch.setattr(models_module, "_validate_connectivity", lambda *a: "Connection refused")
    response = client.post("/models/check-fail/check", headers=admin_headers)
    assert response.status_code == 200
    assert response.json()["is_active"] is False
    assert "Connection refused" in response.json()["connectivity"]


def test_check_model_not_found(client, admin_headers, clean_models):
    response = client.post("/models/nonexistent/check", headers=admin_headers)
    assert response.status_code == 404


# --- Connectivity validation ---


def test_create_model_validates_connectivity(client, admin_headers, clean_models):
    """Model with bad key should be created but inactive."""
    response = _create_model(
        client,
        admin_headers,
        name="bad-key-model",
        provider="openai",
        api_key="sk-invalid-key",
    )
    assert response.status_code == 201
    data = response.json()
    assert data["is_active"] is False  # connectivity failed


def test_connectivity_uses_user_provided_model_name(
    client, admin_headers, clean_models, monkeypatch
):
    """Verify connectivity validation uses the model name provided by user, not a default."""
    captured_model_names = []

    import api.routers.models as models_module

    def mock_validate(provider, model_name, api_key, base_url):
        captured_model_names.append(model_name)
        return None  # success

    monkeypatch.setattr(models_module, "_validate_connectivity", mock_validate)

    response = _create_model(
        client,
        admin_headers,
        name="my-custom-gpt-4o-variant",
        provider="openai",
        api_key="sk-test",
    )
    assert response.status_code == 201
    assert "my-custom-gpt-4o-variant" in captured_model_names


def test_validate_connectivity_timeout(monkeypatch):
    """Timeout returns descriptive error."""
    import httpx
    import api.routers.models as models_module

    monkeypatch.setattr(
        httpx, "post", lambda *a, **kw: (_ for _ in ()).throw(httpx.TimeoutException("timed out"))
    )
    result = models_module._validate_connectivity("openai", "gpt-4", "sk-key", None)
    assert "timed out" in result


def test_validate_connectivity_connect_error(monkeypatch):
    """Connection error returns descriptive error."""
    import httpx
    import api.routers.models as models_module

    monkeypatch.setattr(
        httpx, "post", lambda *a, **kw: (_ for _ in ()).throw(httpx.ConnectError("refused"))
    )
    result = models_module._validate_connectivity("openai", "gpt-4", "sk-key", None)
    assert "Connection failed" in result


def test_validate_connectivity_unexpected_error(monkeypatch):
    """Unexpected error returns descriptive error."""
    import httpx
    import api.routers.models as models_module

    monkeypatch.setattr(httpx, "post", lambda *a, **kw: (_ for _ in ()).throw(RuntimeError("boom")))
    result = models_module._validate_connectivity("openai", "gpt-4", "sk-key", None)
    assert "Unexpected error" in result


def test_validate_connectivity_anthropic_path(monkeypatch):
    """Anthropic provider uses its own endpoint."""
    import httpx
    import api.routers.models as models_module

    class FakeResp:
        status_code = 200

    monkeypatch.setattr(httpx, "post", lambda url, **kw: FakeResp())
    result = models_module._validate_connectivity("anthropic", "claude-3", "sk-key", None)
    assert result is None


def test_validate_connectivity_http_error_json(monkeypatch):
    """HTTP error with JSON body extracts message."""
    import httpx
    import api.routers.models as models_module

    class FakeResp:
        status_code = 401

        def json(self):
            return {"error": {"message": "Invalid API key"}}

        @property
        def text(self):
            return '{"error": {"message": "Invalid API key"}}'

    monkeypatch.setattr(httpx, "post", lambda *a, **kw: FakeResp())
    result = models_module._validate_connectivity("openai", "gpt-4", "sk-key", None)
    assert "401" in result


def test_validate_connectivity_http_error_no_json(monkeypatch):
    """HTTP error with non-JSON body falls back to text."""
    import httpx
    import api.routers.models as models_module

    class FakeResp:
        status_code = 500

        def json(self):
            raise ValueError("not json")

        @property
        def text(self):
            return "Internal Server Error"

    monkeypatch.setattr(httpx, "post", lambda *a, **kw: FakeResp())
    result = models_module._validate_connectivity("openai", "gpt-4", "sk-key", None)
    assert "500" in result
    assert "Internal Server Error" in result


# --- Resolve base_url ---


def test_resolve_base_url_explicit_wins():
    from api.routers.models import _resolve_base_url

    assert _resolve_base_url("deepseek", "https://custom.api/v1") == "https://custom.api/v1"


def test_resolve_base_url_falls_back_to_known():
    from api.routers.models import _resolve_base_url

    result = _resolve_base_url("deepseek", None)
    assert result == "https://api.deepseek.com/v1"


def test_resolve_base_url_unknown_provider():
    from api.routers.models import _resolve_base_url

    assert _resolve_base_url("unknown-provider", None) is None


# --- Sanitize error ---


def test_sanitize_error_redacts_api_key():
    from api.routers.models import _sanitize_error

    result = _sanitize_error("Error with sk-abc123456789012345678901234567890")
    assert "sk-abc123..." in result


# --- API key not exposed ---


def test_api_key_not_in_response(client, admin_headers, clean_models):
    response = _create_model(client, admin_headers, name="secret-model")
    assert response.status_code == 201
    data = response.json()
    assert "api_key" not in data
    assert "api_key_encrypted" not in data
