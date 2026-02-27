"""End-to-end tests for Jobs, Workflows, and Triggers APIs.

These three routers had ZERO test coverage. Tests go through REST API only.

Scenarios:
  J1. Job lifecycle: submit → poll → terminal state
  J2. Job cancel: submit → cancel already-finished → 409
  J3. Job not found: GET/DELETE unknown job → 404
  J4. Job webhook: POST completion webhook → verify response
  J5. Auth enforcement: all job endpoints require auth

  W1. Workflow list: GET /workflows → list
  W2. Workflow run not found: GET /workflows/runs/{id} → 404
  W3. Workflow resolve not found: POST /workflows/runs/{id}/resolve → 404
  W4. Workflow CRUD with seeded data: definition + run → query → resolve
  W5. Auth enforcement: all workflow endpoints require auth

  T1. Trigger lifecycle: create webhook → list → fire → delete
  T2. Trigger schedule: create cron trigger → list → delete
  T3. Trigger fire auth: wrong secret → 403, missing trigger → 404
  T4. Trigger ownership: user A cannot delete user B's trigger
  T5. Trigger validation: invalid type → 400, schedule without cron → 400
  T6. Auth enforcement: all trigger endpoints require auth
"""

from __future__ import annotations

import time
from unittest.mock import patch

import pytest
from fastapi.testclient import TestClient

from core.jobs.backend import JobResult, JobStatus
from core.jobs.local import LocalJobBackend
from core.utils.id_generator import generate_id


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture(autouse=True)
def _no_real_subprocesses():
    """Replace LocalJobBackend._run with instant in-process version.

    Real _run spawns subprocesses via asyncio.create_subprocess_exec.
    Subprocess transports outlive TestClient's event loop → RuntimeError
    in __del__. This mock eliminates subprocesses while preserving the
    full submit → poll → terminal lifecycle through the API.
    """
    async def _fake_run(self, job_id, job_type, inputs, req):
        self._results[job_id] = JobResult(
            job_id=job_id, status=JobStatus.FAILED,
            error=f"Unknown job type: {job_type}",
        )

    with patch.object(LocalJobBackend, "_run", _fake_run):
        yield

    # Clean singleton state so next test starts fresh
    from api.routers.jobs import _router
    backend = _router.backends.get("local")
    if backend:
        backend._tasks.clear()
        backend._results.clear()


@pytest.fixture
def client():
    from api.main import app
    return TestClient(app)


def _make_user(client, suffix=""):
    """Register + login, return auth headers."""
    uid = generate_id()
    # Keep username ≤ 50 chars (VARCHAR(50) constraint)
    username = f"jwt_{uid}{suffix}"[:50]
    client.post("/auth/register", json={
        "username": username,
        "email": f"{uid}@test.com",
        "password": "testpass1234",
    })
    resp = client.post("/auth/login", json={
        "username": username, "password": "testpass1234",
    })
    return {"Authorization": f"Bearer {resp.json()['access_token']}"}


@pytest.fixture
def auth_headers(client):
    return _make_user(client)


# ============================================================================
# Jobs
# ============================================================================

class TestJ1_JobLifecycle:
    """Submit → poll → terminal state. Fake backend fails instantly."""

    def test_submit_and_poll(self, client, auth_headers):
        h = auth_headers

        resp = client.post("/jobs", json={
            "job_type": "nonexistent_type",
            "inputs": {"x": 1},
            "timeout_seconds": 10,
        }, headers=h)
        assert resp.status_code == 200
        data = resp.json()
        assert "job_id" in data
        assert data["status"] == "pending"
        job_id = data["job_id"]

        # Poll until terminal
        deadline = time.monotonic() + 10
        status = None
        while time.monotonic() < deadline:
            resp = client.get(f"/jobs/{job_id}", headers=h)
            assert resp.status_code == 200
            status = resp.json()["status"]
            if status in ("completed", "failed", "cancelled"):
                break
            time.sleep(0.1)

        assert status in ("failed", "cancelled"), f"Expected terminal, got {status}"
        assert resp.json().get("error")


class TestJ2_JobCancel:
    """Cancel an already-finished job → 409 Conflict."""

    def test_cancel_finished_job(self, client, auth_headers):
        h = auth_headers

        resp = client.post("/jobs", json={
            "job_type": "nonexistent_type", "inputs": {},
        }, headers=h)
        assert resp.status_code == 200
        job_id = resp.json()["job_id"]

        # Fake backend completes instantly → cancel gets 409
        resp = client.delete(f"/jobs/{job_id}", headers=h)
        assert resp.status_code == 409
        assert "already" in resp.json()["detail"].lower()


class TestJ3_JobNotFound:
    """GET/DELETE unknown job_id → 404."""

    def test_get_unknown(self, client, auth_headers):
        resp = client.get("/jobs/nonexistent-id", headers=auth_headers)
        assert resp.status_code == 404
        assert "not found" in resp.json()["detail"].lower()

    def test_cancel_unknown(self, client, auth_headers):
        resp = client.delete("/jobs/nonexistent-id", headers=auth_headers)
        assert resp.status_code == 404
        assert "not found" in resp.json()["detail"].lower()


class TestJ4_JobWebhook:
    """POST /jobs/webhook — completion callback (no auth required)."""

    def test_webhook_completion(self, client):
        resp = client.post("/jobs/webhook", json={
            "job_id": "fake-job-123",
            "status": "completed",
            "result": {"accuracy": 0.95},
        })
        assert resp.status_code == 200
        data = resp.json()
        assert data["job_id"] == "fake-job-123"
        assert data["resumed"] is False  # no run was waiting

    def test_webhook_with_error(self, client):
        resp = client.post("/jobs/webhook", json={
            "job_id": "fake-job-456",
            "status": "failed",
            "error": "OOM killed",
        })
        assert resp.status_code == 200
        assert resp.json()["resumed"] is False


class TestJ5_JobAuth:
    """All job endpoints require authentication."""

    def test_submit_no_auth(self, client):
        resp = client.post("/jobs", json={"job_type": "x", "inputs": {}})
        assert resp.status_code == 401

    def test_get_no_auth(self, client):
        resp = client.get("/jobs/some-id")
        assert resp.status_code == 401

    def test_cancel_no_auth(self, client):
        resp = client.delete("/jobs/some-id")
        assert resp.status_code == 401


# ============================================================================
# Workflows
# ============================================================================

class TestW1_WorkflowList:
    """GET /workflows → list."""

    def test_list_workflows(self, client, auth_headers):
        resp = client.get("/workflows", headers=auth_headers)
        assert resp.status_code == 200
        assert isinstance(resp.json(), list)


class TestW2_WorkflowRunNotFound:
    """GET /workflows/runs/{id} → 404."""

    def test_run_not_found(self, client, auth_headers):
        resp = client.get("/workflows/runs/nonexistent-run", headers=auth_headers)
        assert resp.status_code == 404


class TestW3_WorkflowResolveNotFound:
    """POST /workflows/runs/{id}/resolve → 404 when not found or not waiting."""

    def test_resolve_not_found(self, client, auth_headers):
        resp = client.post(
            "/workflows/runs/nonexistent-run/resolve",
            json={"approved": True},
            headers=auth_headers,
        )
        assert resp.status_code == 404


class TestW4_WorkflowCRUD:
    """Seed definition + run in DB → query via API → resolve.

    Write path (creating definitions/runs) is internal, not API-exposed.
    """

    def test_seeded_workflow(self, client, auth_headers):
        from api.database import get_db_session
        from sqlalchemy import text

        h = auth_headers
        wf_id = f"wf-{generate_id()}"
        run_id = f"run-{generate_id()}"

        db = next(get_db_session())
        try:
            db.execute(text(
                "INSERT INTO wf_definitions "
                "(workflow_id, name, version, description, definition, is_active) "
                "VALUES (:wid, :name, :ver, :desc, :defn, 1)"
            ), {
                "wid": wf_id, "name": "approval-flow", "ver": "1.0.0",
                "desc": "Test workflow", "defn": '{"steps": []}',
            })
            db.execute(text(
                "INSERT INTO wf_runs "
                "(run_id, workflow_id, status, current_step_idx, step_results) "
                "VALUES (:rid, :wid, :status, 0, :sr)"
            ), {"rid": run_id, "wid": wf_id, "status": "waiting", "sr": '{}'})
            db.commit()
        finally:
            db.close()

        # List — should include our definition
        resp = client.get("/workflows", headers=h)
        assert resp.status_code == 200
        assert any(w["name"] == "approval-flow" for w in resp.json())

        # Get run
        resp = client.get(f"/workflows/runs/{run_id}", headers=h)
        assert resp.status_code == 200
        data = resp.json()
        assert data["run_id"] == run_id
        assert data["workflow_id"] == wf_id
        assert data["status"] == "waiting"

        # Resolve without wait handle → 400
        resp = client.post(
            f"/workflows/runs/{run_id}/resolve",
            json={"approved": True}, headers=h,
        )
        assert resp.status_code == 400

        # Set wait handle, then resolve → 409 (no in-memory workflow state)
        db = next(get_db_session())
        try:
            db.execute(text(
                "UPDATE wf_runs SET waiting_for = :wf WHERE run_id = :rid"
            ), {"wf": f"approval:{run_id}", "rid": run_id})
            db.commit()
        finally:
            db.close()

        resp = client.post(
            f"/workflows/runs/{run_id}/resolve",
            json={"approved": True}, headers=h,
        )
        assert resp.status_code == 409


class TestW5_WorkflowAuth:
    """All workflow endpoints require authentication."""

    def test_list_no_auth(self, client):
        assert client.get("/workflows").status_code == 401

    def test_get_run_no_auth(self, client):
        assert client.get("/workflows/runs/x").status_code == 401

    def test_resolve_no_auth(self, client):
        assert client.post("/workflows/runs/x/resolve", json={}).status_code == 401


# ============================================================================
# Triggers
# ============================================================================

class TestT1_TriggerWebhookLifecycle:
    """Create webhook → list → fire → delete → verify gone."""

    def test_full_lifecycle(self, client, auth_headers):
        h = auth_headers

        # Create
        resp = client.post("/triggers", json={
            "trigger_type": "webhook",
            "name": "deploy-hook",
            "agent_id": "dev-agent",
            "user_input": "Deploy completed, run post-deploy checks",
        }, headers=h)
        assert resp.status_code == 200
        data = resp.json()
        assert data["trigger_type"] == "webhook"
        assert "secret" in data
        assert "webhook_url" in data
        trigger_id = data["trigger_id"]
        secret = data["secret"]

        # List
        resp = client.get("/triggers", headers=h)
        assert resp.status_code == 200
        assert any(t["trigger_id"] == trigger_id for t in resp.json())

        # Fire (secret-based auth, no JWT)
        resp = client.post(f"/triggers/{trigger_id}/fire", json={
            "secret": secret,
            "payload": {"commit": "abc123"},
        })
        assert resp.status_code == 200
        assert resp.json()["trigger_id"] == trigger_id
        assert "run_id" in resp.json()

        # Delete
        resp = client.delete(f"/triggers/{trigger_id}", headers=h)
        assert resp.status_code == 200
        assert resp.json()["deleted"] is True

        # Verify gone
        resp = client.get("/triggers", headers=h)
        assert not any(t["trigger_id"] == trigger_id for t in resp.json())


class TestT2_TriggerSchedule:
    """Create cron trigger → list → delete."""

    def test_schedule_trigger(self, client, auth_headers):
        h = auth_headers

        resp = client.post("/triggers", json={
            "trigger_type": "schedule",
            "name": "nightly-eval",
            "agent_id": "dev-agent",
            "user_input": "Run nightly evaluation",
            "cron_expr": "0 2 * * *",
        }, headers=h)
        assert resp.status_code == 200
        data = resp.json()
        assert data["trigger_type"] == "schedule"
        assert "secret" not in data
        assert "next_fire_at" in data
        trigger_id = data["trigger_id"]

        resp = client.get("/triggers", headers=h)
        assert any(t["trigger_id"] == trigger_id for t in resp.json())

        resp = client.delete(f"/triggers/{trigger_id}", headers=h)
        assert resp.status_code == 200


class TestT3_TriggerFireAuth:
    """Wrong secret → 403, missing trigger → 404."""

    def test_wrong_secret(self, client, auth_headers):
        h = auth_headers
        resp = client.post("/triggers", json={
            "trigger_type": "webhook", "name": "secret-test",
            "agent_id": "dev-agent", "user_input": "test",
        }, headers=h)
        trigger_id = resp.json()["trigger_id"]

        resp = client.post(f"/triggers/{trigger_id}/fire", json={
            "secret": "wrong-secret-value",
        })
        assert resp.status_code == 403

        client.delete(f"/triggers/{trigger_id}", headers=h)

    def test_fire_nonexistent(self, client):
        resp = client.post("/triggers/nonexistent/fire", json={"secret": "x"})
        assert resp.status_code == 404


class TestT4_TriggerOwnership:
    """User A cannot delete user B's trigger."""

    def test_cross_user_delete_forbidden(self, client):
        h_a = _make_user(client, "_a")
        h_b = _make_user(client, "_b")

        resp = client.post("/triggers", json={
            "trigger_type": "webhook", "name": "owned-by-a",
            "agent_id": "dev-agent", "user_input": "test",
        }, headers=h_a)
        trigger_id = resp.json()["trigger_id"]

        # User B → 403
        assert client.delete(f"/triggers/{trigger_id}", headers=h_b).status_code == 403
        # User A → 200
        assert client.delete(f"/triggers/{trigger_id}", headers=h_a).status_code == 200


class TestT5_TriggerValidation:
    """Input validation: invalid type, missing cron, bad cron."""

    def test_invalid_trigger_type(self, client, auth_headers):
        resp = client.post("/triggers", json={
            "trigger_type": "invalid_type", "name": "bad",
            "agent_id": "dev-agent", "user_input": "test",
        }, headers=auth_headers)
        assert resp.status_code == 400

    def test_schedule_without_cron(self, client, auth_headers):
        resp = client.post("/triggers", json={
            "trigger_type": "schedule", "name": "no-cron",
            "agent_id": "dev-agent", "user_input": "test",
        }, headers=auth_headers)
        assert resp.status_code == 400

    def test_invalid_cron_expression(self, client, auth_headers):
        resp = client.post("/triggers", json={
            "trigger_type": "schedule", "name": "bad-cron",
            "agent_id": "dev-agent", "user_input": "test",
            "cron_expr": "not a cron",
        }, headers=auth_headers)
        assert resp.status_code == 400


class TestT6_TriggerAuth:
    """All trigger management endpoints require auth (fire uses secret)."""

    def test_create_no_auth(self, client):
        resp = client.post("/triggers", json={
            "trigger_type": "webhook", "name": "x",
            "agent_id": "a", "user_input": "y",
        })
        assert resp.status_code == 401

    def test_list_no_auth(self, client):
        assert client.get("/triggers").status_code == 401

    def test_delete_no_auth(self, client):
        assert client.delete("/triggers/some-id").status_code == 401
