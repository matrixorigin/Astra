"""End-to-end tests for model selection with real chat flow using mock provider."""

import pytest
from fastapi.testclient import TestClient
from sqlalchemy import text
import json
import time
from api.main import app
from api.database import get_db_session


@pytest.fixture
def client():
    """Create test client without database override to avoid concurrency issues.

    Uses context manager so the event loop persists across requests — required
    for background tasks created via asyncio.create_task (e.g. RunEngine).
    """
    import os
    os.environ['DISABLE_GATE_TRIGGER'] = '1'
    
    with TestClient(app) as c:
        yield c
    
    # Cleanup
    os.environ.pop('DISABLE_GATE_TRIGGER', None)


@pytest.fixture
def auth_headers_e2e(client):
    """Get auth headers with admin role for E2E tests."""
    import uuid
    from api.database import get_db_session
    from core.auth.seed_roles import seed_roles
    from sqlalchemy import text

    username = f"e2e_user_{uuid.uuid4().hex}"
    
    # Ensure roles exist (parallel workers may not have seeded yet)
    db = next(get_db_session())
    seed_roles(db)
    db.close()

    resp = client.post(
        "/auth/register",
        json={
            "username": username,
            "password": "testpass123",
            "email": f"{username}@test.com",
        }
    )
    user_id = resp.json()["user_id"]
    
    # Grant admin role
    db = next(get_db_session())
    role = db.execute(text("SELECT role_id FROM auth_roles WHERE role_name = 'mo_agent_admin' LIMIT 1")).fetchone()
    if role:
        db.execute(text("INSERT INTO auth_user_roles (user_id, role_id) VALUES (:uid, :rid)"), {"uid": user_id, "rid": role[0]})
        db.commit()
    db.close()
    
    # Login
    response = client.post(
        "/auth/login",
        json={"username": username, "password": "testpass123"}
    )
    
    token = response.json()["access_token"]
    return {"Authorization": f"Bearer {token}"}


@pytest.fixture(autouse=True)
def cleanup_models(client, auth_headers_e2e):
    """Clean up all mock models before and after each test."""
    # Cleanup before test
    models_resp = client.get("/models", headers=auth_headers_e2e)
    if models_resp.status_code == 200:
        for model in models_resp.json():
            if model["name"].startswith("mock-"):
                client.delete(f"/models/{model['name']}?scope=global", headers=auth_headers_e2e)
    
    yield
    
    # Cleanup after test
    models_resp = client.get("/models", headers=auth_headers_e2e)
    if models_resp.status_code == 200:
        for model in models_resp.json():
            if model["name"].startswith("mock-"):
                client.delete(f"/models/{model['name']}?scope=global", headers=auth_headers_e2e)


@pytest.fixture
def setup_mock_model(client, auth_headers_e2e):
    """Setup mock echo model via API."""
    # Add mock model via API
    response = client.post(
        "/models",
        headers=auth_headers_e2e,
        json={
            "name": "mock-echo",
            "provider": "mock", "api_key": "test-key",
            
            "context_window": 128000,
            "tags": ["test"],
        }
    )
    # Should succeed (201) or already exist (400)
    assert response.status_code in [201, 400], f"Failed: {response.text}"
    
    yield


def test_e2e_chat_with_mock_model(client, auth_headers_e2e, setup_mock_model):
    """Test complete chat flow with mock echo model."""
    # Just verify the API accepts the model parameter
    # Full execution is tested in other tests
    response = client.post(
        "/chat",
        headers=auth_headers_e2e,
        json={
            "message": "Hello World",
            "model": "mock-echo",
        }
    )
    
    assert response.status_code == 200
    data = response.json()
    assert "run_id" in data
    assert "session_id" in data


def test_e2e_chat_stream_with_mock_model(client, auth_headers_e2e, setup_mock_model):
    """Test streaming chat with mock echo model."""
    response = client.post(
        "/chat/stream",
        headers=auth_headers_e2e,
        json={
            "message": "Test streaming",
            "model": "mock-echo",
        }
    )
    
    # Just verify streaming starts successfully
    assert response.status_code == 200


def test_e2e_model_selection_persists_in_session(client, auth_headers_e2e, setup_mock_model):
    """Test that model selection persists across multiple messages in same session."""
    # First message with model selection
    response1 = client.post(
        "/chat",
        headers=auth_headers_e2e,
        json={
            "message": "First message",
            "model": "mock-echo",
        }
    )
    
    assert response1.status_code == 200
    session_id = response1.json()["session_id"]
    
    # Second message in same session with same model
    response2 = client.post(
        "/chat",
        headers=auth_headers_e2e,
        json={
            "message": "Second message",
            "model": "mock-echo",
            "session_id": session_id,
        }
    )
    
    assert response2.status_code == 200
    assert response2.json()["session_id"] == session_id


def test_e2e_different_models_in_different_sessions(client, auth_headers_e2e):
    """Test using different models in different sessions."""
    # Register two models via API
    client.post(
        "/models",
        headers=auth_headers_e2e,
        json={"name": "mock-echo-1", "provider": "mock", "api_key": "test-key"}
    )
    client.post(
        "/models",
        headers=auth_headers_e2e,
        json={"name": "mock-echo-2", "provider": "mock", "api_key": "test-key"}
    )
    
    # Session 1 with model 1
    response1 = client.post(
        "/chat",
        headers=auth_headers_e2e,
        json={"message": "Message 1", "model": "mock-echo-1"}
    )
    assert response1.status_code == 200
    
    # Session 2 with model 2
    response2 = client.post(
        "/chat",
        headers=auth_headers_e2e,
        json={"message": "Message 2", "model": "mock-echo-2"}
    )
    assert response2.status_code == 200
    
    # Different sessions
    assert response1.json()["session_id"] != response2.json()["session_id"]
    
    # Cleanup
    client.delete("/models/mock-echo-1?scope=global", headers=auth_headers_e2e)
    client.delete("/models/mock-echo-2?scope=global", headers=auth_headers_e2e)


def test_e2e_list_models_shows_mock_model(client, auth_headers_e2e, setup_mock_model):
    """Test that /models API shows registered mock model."""
    response = client.get("/models", headers=auth_headers_e2e)
    
    assert response.status_code == 200
    models = response.json()
    
    mock_models = [m for m in models if m["name"] == "mock-echo"]
    assert len(mock_models) == 1
    assert mock_models[0]["provider"] == "mock"


# ── Bug regression tests ──────────────────────────────────────


def test_e2e_run_completes_with_mock_model(client, auth_headers_e2e, setup_mock_model):
    """Regression: RunEngine must use dedicated DB session so runs actually complete."""
    response = client.post(
        "/chat",
        headers=auth_headers_e2e,
        json={"message": "Hello", "model": "mock-echo"}
    )
    assert response.status_code == 200
    run_id = response.json()["run_id"]
    
    # Poll for completion — this used to fail with "session is provisioning"
    for _ in range(30):
        status_resp = client.get(f"/chat/runs/{run_id}", headers=auth_headers_e2e)
        status = status_resp.json()["status"]
        if status in ["completed", "failed"]:
            break
        time.sleep(0.1)
    
    assert status == "completed", f"Run should complete but got: {status}"


def test_e2e_echo_response_content(client, auth_headers_e2e, setup_mock_model):
    """Regression: MockEchoProvider must echo user message through full pipeline."""
    response = client.post(
        "/chat",
        headers=auth_headers_e2e,
        json={"message": "ping", "model": "mock-echo"}
    )
    run_id = response.json()["run_id"]
    
    # Wait for completion
    for _ in range(30):
        status_resp = client.get(f"/chat/runs/{run_id}", headers=auth_headers_e2e)
        if status_resp.json()["status"] in ["completed", "failed"]:
            break
        time.sleep(0.1)
    
    # Verify echo content in events
    events_resp = client.get(f"/chat/runs/{run_id}/stream", headers=auth_headers_e2e)
    assert "Echo:" in events_resp.text and "ping" in events_resp.text


def test_e2e_model_parameter_reaches_llm(client, auth_headers_e2e, setup_mock_model):
    """Regression: model from /chat request.model must reach LLMClient (was ignored before)."""
    response = client.post(
        "/chat",
        headers=auth_headers_e2e,
        json={"message": "test", "model": "mock-echo"}
    )
    run_id = response.json()["run_id"]
    
    # Wait for completion
    for _ in range(30):
        status_resp = client.get(f"/chat/runs/{run_id}", headers=auth_headers_e2e)
        if status_resp.json()["status"] in ["completed", "failed"]:
            break
        time.sleep(0.1)
    
    # If model wasn't passed, it would try gpt-4o and fail
    assert status_resp.json()["status"] == "completed"


def test_e2e_provider_string_value_no_crash(client, auth_headers_e2e, setup_mock_model):
    """Regression: provider stored as string in DB must not crash on .value access."""
    # This used to crash with: AttributeError: 'str' object has no attribute 'value'
    response = client.post(
        "/chat",
        headers=auth_headers_e2e,
        json={"message": "test", "model": "mock-echo"}
    )
    run_id = response.json()["run_id"]
    
    for _ in range(30):
        status_resp = client.get(f"/chat/runs/{run_id}", headers=auth_headers_e2e)
        if status_resp.json()["status"] in ["completed", "failed"]:
            break
        time.sleep(0.1)
    
    assert status_resp.json()["status"] == "completed"
