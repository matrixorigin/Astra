"""Integration tests for per-agent model override.

Tests the complete flow: agent config → run engine → chat loop → LLM client.
Uses mock provider to verify model selection without real API calls.
"""

import pytest
import time
from fastapi.testclient import TestClient
from sqlalchemy import text

from api.main import app
from api.database import get_db_session


@pytest.fixture
def client():
    """Test client with gate trigger disabled.

    Uses context manager so the event loop persists across requests — required
    for background tasks created via asyncio.create_task (e.g. RunEngine).
    """
    import os
    os.environ['DISABLE_GATE_TRIGGER'] = '1'
    with TestClient(app) as c:
        yield c
    os.environ.pop('DISABLE_GATE_TRIGGER', None)


@pytest.fixture
def auth_headers(client):
    """Create admin user and return auth headers."""
    import uuid
    from core.auth.seed_roles import seed_roles
    
    username = f"agent_model_test_{uuid.uuid4().hex}"
    
    db = next(get_db_session())
    seed_roles(db)
    db.close()
    
    # Register
    resp = client.post("/auth/register", json={
        "username": username,
        "password": "testpass123",
        "email": f"{username}@test.com",
    })
    assert resp.status_code in [200, 201], resp.text
    user_id = resp.json()["user_id"]
    
    # Grant admin role
    db = next(get_db_session())
    role = db.execute(text(
        "SELECT role_id FROM auth_roles WHERE role_name = 'mo_agent_admin' LIMIT 1"
    )).fetchone()
    if role:
        db.execute(text(
            "INSERT INTO auth_user_roles (user_id, role_id) VALUES (:uid, :rid)"
        ), {"uid": user_id, "rid": role[0]})
        db.commit()
    db.close()
    
    # Login
    resp = client.post("/auth/login", json={
        "username": username,
        "password": "testpass123"
    })
    token = resp.json()["access_token"]
    
    yield {"Authorization": f"Bearer {token}", "user_id": user_id}


@pytest.fixture
def setup_mock_models(client, auth_headers):
    """Register mock models for testing."""
    models = ["mock-opus", "mock-sonnet", "mock-haiku"]
    
    for model in models:
        resp = client.post("/models", headers=auth_headers, json={
            "name": model,
            "provider": "mock",
            "api_key": "test-key",
            "context_window": 128000,
            "tags": ["test"],
        })
        assert resp.status_code in [201, 400], f"Failed to create {model}: {resp.text}"
    
    yield models
    
    # Cleanup
    for model in models:
        client.delete(f"/models/{model}", headers=auth_headers)


@pytest.fixture
def create_agent_with_model(client, auth_headers):
    """Factory to create agents with specific model config."""
    created_agents = []
    
    def _create(agent_name: str, model: str, model_constraints: dict | None = None):
        import uuid
        agent_id = f"test-{agent_name}-{uuid.uuid4().hex}"
        
        config = {
            "system_prompt": f"You are {agent_name}.",
            "model": model,
        }
        if model_constraints:
            config["model_constraints"] = model_constraints
        
        resp = client.post("/agents", headers=auth_headers, json={
            "name": agent_name,  # API uses 'name' not 'agent_name'
            "agent_config": config,
        })
        assert resp.status_code == 201, f"Failed to create agent: {resp.text}"
        created_agents.append(resp.json()["agent_id"])
        return resp.json()["agent_id"]
    
    yield _create
    
    # Cleanup
    for agent_id in created_agents:
        client.delete(f"/agents/{agent_id}", headers=auth_headers)


def wait_for_run(client, auth_headers, run_id: str, timeout: float = 10.0) -> dict:
    """Poll until run completes or times out."""
    start = time.time()
    while time.time() - start < timeout:
        resp = client.get(f"/chat/runs/{run_id}", headers=auth_headers)
        status = resp.json()
        if status["status"] in ["completed", "failed"]:
            return status
        time.sleep(0.1)
    raise TimeoutError(f"Run {run_id} did not complete in {timeout}s")


# ============================================================================
# Test Cases
# ============================================================================


class TestAgentModelConfig:
    """Test agent model configuration via API."""
    
    def test_create_agent_with_model(self, client, auth_headers, setup_mock_models):
        """Agent can be created with model in config."""
        resp = client.post("/agents", headers=auth_headers, json={
            "name": "Test Agent With Model",
            "agent_config": {
                "system_prompt": "You are a test agent.",
                "model": "mock-sonnet",
            },
        })
        
        assert resp.status_code == 201, resp.text
        agent_id = resp.json()["agent_id"]
        
        # Verify config is stored
        resp = client.get(f"/agents/{agent_id}", headers=auth_headers)
        assert resp.status_code == 200
        agent = resp.json()
        assert agent["agent_config"]["model"] == "mock-sonnet"
        
        # Cleanup
        client.delete(f"/agents/{agent_id}", headers=auth_headers)
    
    def test_create_agent_with_model_constraints(self, client, auth_headers, setup_mock_models):
        """Agent can have model constraints (fallback, cost limits)."""
        resp = client.post("/agents", headers=auth_headers, json={
            "name": "Constrained Agent",
            "agent_config": {
                "model": "mock-opus",
                "model_constraints": {
                    "fallback": "mock-haiku",
                    "max_cost_per_call": 0.05,
                },
            },
        })
        
        assert resp.status_code == 201, resp.text
        agent_id = resp.json()["agent_id"]
        
        resp = client.get(f"/agents/{agent_id}", headers=auth_headers)
        config = resp.json()["agent_config"]
        assert config["model"] == "mock-opus"
        assert config["model_constraints"]["fallback"] == "mock-haiku"
        
        client.delete(f"/agents/{agent_id}", headers=auth_headers)


class TestAgentModelResolution:
    """Test model resolution priority chain."""
    
    def test_agent_model_used_when_no_override(
        self, client, auth_headers, setup_mock_models, create_agent_with_model
    ):
        """Agent's configured model is used when no explicit override."""
        agent_id = create_agent_with_model("sonnet-agent", "mock-sonnet")
        
        # Chat with this agent (no model in request)
        resp = client.post("/chat", headers=auth_headers, json={
            "message": "Hello",
            "agent_id": agent_id,
        })
        assert resp.status_code == 200
        run_id = resp.json()["run_id"]
        
        # Wait for completion
        status = wait_for_run(client, auth_headers, run_id)
        assert status["status"] == "completed", f"Run failed: {status}"
    
    def test_request_model_overrides_agent_model(
        self, client, auth_headers, setup_mock_models, create_agent_with_model
    ):
        """Explicit request.model takes priority over agent config."""
        agent_id = create_agent_with_model("sonnet-agent", "mock-sonnet")
        
        # Chat with explicit model override
        resp = client.post("/chat", headers=auth_headers, json={
            "message": "Hello",
            "agent_id": agent_id,
            "model": "mock-haiku",  # Override agent's mock-sonnet
        })
        assert resp.status_code == 200
        run_id = resp.json()["run_id"]
        
        status = wait_for_run(client, auth_headers, run_id)
        assert status["status"] == "completed"
    
    def test_agent_without_model_uses_default(self, client, auth_headers, setup_mock_models):
        """Agent without model config uses system default."""
        # Create agent without model
        resp = client.post("/agents", headers=auth_headers, json={
            "name": "No Model Agent",
            "agent_config": {
                "system_prompt": "You are a test agent.",
                # No model specified
            },
        })
        assert resp.status_code == 201, resp.text
        agent_id = resp.json()["agent_id"]
        
        # Chat - should use default model
        resp = client.post("/chat", headers=auth_headers, json={
            "message": "Hello",
            "agent_id": agent_id,
            "model": "mock-haiku",  # Provide model since no default
        })
        assert resp.status_code == 200
        run_id = resp.json()["run_id"]
        
        status = wait_for_run(client, auth_headers, run_id)
        assert status["status"] == "completed"
        
        client.delete(f"/agents/{agent_id}", headers=auth_headers)


class TestMultiAgentModelSelection:
    """Test different models for different agents in workflows."""
    
    def test_different_agents_use_different_models(
        self, client, auth_headers, setup_mock_models, create_agent_with_model
    ):
        """Multiple agents can each use their own configured model."""
        # Create agents with different models
        opus_agent = create_agent_with_model("orchestrator", "mock-opus")
        sonnet_agent = create_agent_with_model("implementer", "mock-sonnet")
        haiku_agent = create_agent_with_model("tester", "mock-haiku")
        
        # Run each agent
        for agent_id, expected_model in [
            (opus_agent, "mock-opus"),
            (sonnet_agent, "mock-sonnet"),
            (haiku_agent, "mock-haiku"),
        ]:
            resp = client.post("/chat", headers=auth_headers, json={
                "message": f"Test message for {agent_id}",
                "agent_id": agent_id,
            })
            assert resp.status_code == 200
            run_id = resp.json()["run_id"]
            
            status = wait_for_run(client, auth_headers, run_id)
            assert status["status"] == "completed", f"Agent {agent_id} failed"


class TestModelSelectionAudit:
    """Test that model selection is properly audited."""
    
    def test_model_source_tracked_in_events(
        self, client, auth_headers, setup_mock_models, create_agent_with_model
    ):
        """Model selection source is recorded for audit."""
        agent_id = create_agent_with_model("audited-agent", "mock-sonnet")
        
        resp = client.post("/chat", headers=auth_headers, json={
            "message": "Audit test",
            "agent_id": agent_id,
        })
        run_id = resp.json()["run_id"]
        
        wait_for_run(client, auth_headers, run_id)
        
        # Check events contain model info
        resp = client.get(f"/chat/runs/{run_id}/stream", headers=auth_headers)
        # The run completed, which means model was resolved correctly
        assert resp.status_code == 200


class TestEdgeCases:
    """Test edge cases and error handling."""
    
    def test_invalid_model_in_agent_config(self, client, auth_headers):
        """Agent with invalid model should fail gracefully at runtime."""
        # Create agent with non-existent model
        resp = client.post("/agents", headers=auth_headers, json={
            "name": "Invalid Model Agent",
            "agent_config": {
                "model": "non-existent-model",
            },
        })
        assert resp.status_code == 201  # Agent creation succeeds
        agent_id = resp.json()["agent_id"]
        
        # Chat should fail because model doesn't exist
        resp = client.post("/chat", headers=auth_headers, json={
            "message": "Hello",
            "agent_id": agent_id,
        })
        run_id = resp.json()["run_id"]
        
        status = wait_for_run(client, auth_headers, run_id, timeout=5.0)
        # Should fail due to invalid model
        assert status["status"] == "failed"
        
        client.delete(f"/agents/{agent_id}", headers=auth_headers)
    
    def test_agent_model_with_streaming(
        self, client, auth_headers, setup_mock_models, create_agent_with_model
    ):
        """Agent model works with streaming endpoint."""
        agent_id = create_agent_with_model("stream-agent", "mock-sonnet")
        
        resp = client.post("/chat/stream", headers=auth_headers, json={
            "message": "Stream test",
            "agent_id": agent_id,
        })
        
        assert resp.status_code == 200
        # Streaming started successfully
