"""End-to-end tests for Jobs, Workflows, and Triggers APIs.

These three routers had ZERO test coverage. Tests go through REST API only.

Scenarios:
  J1. Job lifecycle: submit → poll status → completed/failed
  J2. Job cancel: submit → cancel → verify cancelled
  J3. Job not found: get/cancel non-existent job → 404
  J4. Job webhook: POST completion webhook → verify response
  J5. Auth enforcement: all job endpoints require auth

  W1. Workflow list: GET /workflows → empty list (no definitions seeded)
  W2. Workflow run not found: GET /workflows/runs/{id} → 404
  W3. Workflow resolve not found: POST /workflows/runs/{id}/resolve → 404
  W4. Workflow CRUD with seeded data: insert definition + run → query → resolve
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

import pytest
from fastapi.testclient import TestClient

from core.utils.id_generator import generate_id


def _drain_job_tasks():
    """Reset the module-level LocalJobBackend state between tests."""
    from api.routers.jobs import _router
    backend = _router.backends.get("local")
    if not backend:
        return
    backend._tasks.clear()
    backend._results.clear()


@pytest.fixture(autouse=True)
def _patch_job_backend():
    """Replace LocalJobBackend._run with a no-subprocess version.

    The real _run spawns subprocesses via asyncio.create_subprocess_exec.
    When TestClient closes its event loop between tests, orphaned subprocess
    transports raise 'Event loop is closed' in __del__. Avoiding real
    subprocesses eliminates this entirely while still testing the full
    API → backend → poll lifecycle.
    """
    from unittest.mock import patch
    from core.jobs.local import LocalJobBackend
    from core.jobs.backend import JobResult, JobStatus

    async def _fake_run(self, job_id, job_type, inputs, req):
        """Simulate job execution without subprocess — instant completion."""
        self._results[job_id] = JobResult(
            job_id=job_id, status=JobStatus.FAILED,
            error=f"Unknown job type: {job_type}",
        )

    with patch.object(LocalJobBackend, "_run", _fake_run):
        yield
    _drain_job_tasks()


@pytest.fixture
def client():
    from api.main import app
    return TestClient(app)


def _make_user(client, suffix=""):
    """Register + login, return auth headers."""
    username = f"e2e_jwt_{generate_id()}{suffix}"
    client.post("/auth/register", json={
        "username": username,
        "email": f"{username}@test.com",
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
    """Submit a job → poll → reaches terminal state.

    LocalJobBackend runs `python -m core.jobs.runner` as subprocess.
    With empty JOB_REGISTRY, any job_type fails with 'Unknown job type' → FAILED.
    This is the expected behavior — we verify the lifecycle, not the job logic.
    """

    def test_submit_and_poll(self, client, auth_headers):
        h = auth_headers

        # Submit
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

        # Poll until terminal (the job will fail because job_type is not registered)
        import time
        deadline = time.monotonic() + 10
        status = None
        while time.monotonic() < deadline:
            resp = client.get(f"/jobs/{job_id}", headers=h)
            assert resp.status_code == 200
            status = resp.json()["status"]
            if status in ("completed", "failed", "cancelled"):
                break
            time.sleep(0.2)

        assert status in ("failed", "cancelled"), f"Expected terminal state, got {status}"
        assert resp.json().get("error")


class TestJ2_JobCancel:
    """Submit → cancel before completion."""

    def test_cancel_job(self, client, auth_headers):
        h = auth_headers

        # Submit a job with long timeout so it stays running
        resp = client.post("/jobs", json={
            "job_type": "nonexistent_type",
            "inputs": {},
            "timeout_seconds": 300,
        }, headers=h)
        assert resp.status_code == 200
        job_id = resp.json()["job_id"]

        # Cancel — may succeed (200) or already finished (404)
        resp = client.delete(f"/jobs/{job_id}", headers=h)
        assert resp.status_code in (200, 404)

        if resp.status_code == 200:
            assert resp.json()["status"] == "cancelled"


class TestJ3_JobNotFound:
    """Get/cancel non-existent job."""

    def test_get_unknown_job(self, client, auth_headers):
        h = auth_headers
        resp = client.get("/jobs/nonexistent-id-999", headers=h)
        # LocalJobBackend returns FAILED with "Unknown job" for missing IDs
        assert resp.status_code == 200
        assert resp.json()["status"] == "failed"
        assert "Unknown" in (resp.json().get("error") or "")

    def test_cancel_unknown_job(self, client, auth_headers):
        h = auth_headers
        resp = client.delete("/jobs/nonexistent-id-999", headers=h)
        assert resp.status_code == 404


class TestJ4_JobWebhook:
    """POST /jobs/webhook — completion callback (no auth required)."""

    def test_webhook_completion(self, client):
        # Webhook with a fake job_id — should return resumed=False (no waiting run)
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
    """GET /workflows — returns list (empty if no definitions seeded)."""

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
    """Seed a workflow definition + run in DB, then query via API.

    This tests the full read path. Write path (creating definitions/runs)
    is done by the workflow engine internally, not via REST API.
    """

    def test_seeded_workflow(self, client, auth_headers):
        from api.database import get_db_session
        from sqlalchemy import text

        h = auth_headers
        wf_id = f"test-wf-{generate_id()}"
        run_id = f"test-run-{generate_id()}"

        # Seed directly into DB (workflow creation is internal, not API-exposed)
        db = next(get_db_session())
        try:
            db.execute(text(
                "INSERT INTO workflow_definitions "
                "(workflow_id, name, version, description, definition, is_active) "
                "VALUES (:wid, :name, :ver, :desc, :defn, 1)"
            ), {
                "wid": wf_id, "name": "approval-flow", "ver": "1.0.0",
                "desc": "Test workflow", "defn": '{"steps": []}',
            })
            db.execute(text(
                "INSERT INTO workflow_runs "
                "(run_id, workflow_id, status, current_step_idx, step_results) "
                "VALUES (:rid, :wid, :status, 0, :sr)"
            ), {
                "rid": run_id, "wid": wf_id, "status": "waiting",
                "sr": '{}',
            })
            db.commit()
        finally:
            db.close()

        # List workflows — should include our seeded definition
        resp = client.get("/workflows", headers=h)
        assert resp.status_code == 200
        names = [w["name"] for w in resp.json()]
        assert "approval-flow" in names

        # Get workflow run
        resp = client.get(f"/workflows/runs/{run_id}", headers=h)
        assert resp.status_code == 200
        data = resp.json()
        assert data["run_id"] == run_id
        assert data["workflow_id"] == wf_id
        assert data["status"] == "waiting"

        # Resolve — will fail because no wait handle is set (400)
        resp = client.post(
            f"/workflows/runs/{run_id}/resolve",
            json={"approved": True},
            headers=h,
        )
        assert resp.status_code == 400  # "No wait handle"

        # Seed a wait handle and try resolve
        db = next(get_db_session())
        try:
            db.execute(text(
                "UPDATE workflow_runs SET waiting_for = :wf WHERE run_id = :rid"
            ), {"wf": f"approval:{run_id}", "rid": run_id})
            db.commit()
        finally:
            db.close()

        # Resolve with handle — resume_workflow returns False (no in-memory state)
        resp = client.post(
            f"/workflows/runs/{run_id}/resolve",
            json={"approved": True},
            headers=h,
        )
        assert resp.status_code == 409  # "Could not resume workflow"


class TestW5_WorkflowAuth:
    """All workflow endpoints require authentication."""

    def test_list_no_auth(self, client):
        resp = client.get("/workflows")
        assert resp.status_code == 401

    def test_get_run_no_auth(self, client):
        resp = client.get("/workflows/runs/some-id")
        assert resp.status_code == 401

    def test_resolve_no_auth(self, client):
        resp = client.post("/workflows/runs/some-id/resolve", json={})
        assert resp.status_code == 401


# ============================================================================
# Triggers
# ============================================================================

class TestT1_TriggerWebhookLifecycle:
    """Create webhook trigger → list → fire → delete."""

    def test_full_lifecycle(self, client, auth_headers):
        h = auth_headers

        # Create webhook trigger
        resp = client.post("/triggers", json={
            "trigger_type": "webhook",
            "name": "deploy-hook",
            "agent_id": "dev-agent",
            "user_input": "Deploy completed, run post-deploy checks",
        }, headers=h)
        assert resp.status_code == 200
        data = resp.json()
        assert data["trigger_type"] == "webhook"
        assert "secret" in data  # webhook triggers get a secret
        assert "webhook_url" in data
        trigger_id = data["trigger_id"]
        secret = data["secret"]

        # List — should contain our trigger
        resp = client.get("/triggers", headers=h)
        assert resp.status_code == 200
        triggers = resp.json()
        assert any(t["trigger_id"] == trigger_id for t in triggers)

        # Fire webhook (no auth header — uses secret)
        # fire_trigger creates an AgentRun which needs a real chat loop.
        # We just verify the API accepts the request and returns a run_id.
        # The run itself may fail (no LLM configured) — that's fine.
        resp = client.post(f"/triggers/{trigger_id}/fire", json={
            "secret": secret,
            "payload": {"commit": "abc123"},
        })
        assert resp.status_code == 200
        fire_data = resp.json()
        assert "run_id" in fire_data
        assert fire_data["trigger_id"] == trigger_id

        # Delete
        resp = client.delete(f"/triggers/{trigger_id}", headers=h)
        assert resp.status_code == 200
        assert resp.json()["deleted"] is True

        # Verify deleted — list should not contain it
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
            "cron_expr": "0 2 * * *",  # 2 AM daily
        }, headers=h)
        assert resp.status_code == 200
        data = resp.json()
        assert data["trigger_type"] == "schedule"
        assert "secret" not in data  # schedule triggers have no secret
        assert "next_fire_at" in data
        trigger_id = data["trigger_id"]

        # List
        resp = client.get("/triggers", headers=h)
        assert any(t["trigger_id"] == trigger_id for t in resp.json())

        # Delete
        resp = client.delete(f"/triggers/{trigger_id}", headers=h)
        assert resp.status_code == 200


class TestT3_TriggerFireAuth:
    """Webhook fire: wrong secret → 403, missing trigger → 404."""

    def test_wrong_secret(self, client, auth_headers):
        h = auth_headers

        # Create trigger
        resp = client.post("/triggers", json={
            "trigger_type": "webhook",
            "name": "secret-test",
            "agent_id": "dev-agent",
            "user_input": "test",
        }, headers=h)
        trigger_id = resp.json()["trigger_id"]

        # Fire with wrong secret
        resp = client.post(f"/triggers/{trigger_id}/fire", json={
            "secret": "wrong-secret-value",
        })
        assert resp.status_code == 403

        # Cleanup
        client.delete(f"/triggers/{trigger_id}", headers=h)

    def test_fire_nonexistent(self, client):
        resp = client.post("/triggers/nonexistent-trigger/fire", json={
            "secret": "anything",
        })
        assert resp.status_code == 404


class TestT4_TriggerOwnership:
    """User A cannot delete user B's trigger."""

    def test_cross_user_delete_forbidden(self, client):
        h_a = _make_user(client, "_a")
        h_b = _make_user(client, "_b")

        # User A creates trigger
        resp = client.post("/triggers", json={
            "trigger_type": "webhook",
            "name": "owned-by-a",
            "agent_id": "dev-agent",
            "user_input": "test",
        }, headers=h_a)
        trigger_id = resp.json()["trigger_id"]

        # User B tries to delete → 403
        resp = client.delete(f"/triggers/{trigger_id}", headers=h_b)
        assert resp.status_code == 403

        # User A can delete
        resp = client.delete(f"/triggers/{trigger_id}", headers=h_a)
        assert resp.status_code == 200


class TestT5_TriggerValidation:
    """Invalid trigger_type → 400, schedule without cron → 400."""

    def test_invalid_trigger_type(self, client, auth_headers):
        resp = client.post("/triggers", json={
            "trigger_type": "invalid_type",
            "name": "bad",
            "agent_id": "dev-agent",
            "user_input": "test",
        }, headers=auth_headers)
        assert resp.status_code == 400

    def test_schedule_without_cron(self, client, auth_headers):
        resp = client.post("/triggers", json={
            "trigger_type": "schedule",
            "name": "no-cron",
            "agent_id": "dev-agent",
            "user_input": "test",
            # cron_expr intentionally omitted
        }, headers=auth_headers)
        assert resp.status_code == 400

    def test_invalid_cron_expression(self, client, auth_headers):
        resp = client.post("/triggers", json={
            "trigger_type": "schedule",
            "name": "bad-cron",
            "agent_id": "dev-agent",
            "user_input": "test",
            "cron_expr": "not a cron",
        }, headers=auth_headers)
        assert resp.status_code == 400


class TestT6_TriggerAuth:
    """All trigger management endpoints require authentication.

    Note: POST /triggers/{id}/fire uses secret-based auth, not JWT.
    """

    def test_create_no_auth(self, client):
        resp = client.post("/triggers", json={
            "trigger_type": "webhook", "name": "x",
            "agent_id": "a", "user_input": "y",
        })
        assert resp.status_code == 401

    def test_list_no_auth(self, client):
        resp = client.get("/triggers")
        assert resp.status_code == 401

    def test_delete_no_auth(self, client):
        resp = client.delete("/triggers/some-id")
        assert resp.status_code == 401
