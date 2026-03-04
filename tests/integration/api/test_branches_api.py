"""Integration tests for Branch API endpoints."""

import uuid

import pytest
from fastapi.testclient import TestClient


@pytest.fixture
def client():
    from api.main import app
    return TestClient(app)


@pytest.fixture(autouse=True)
def setup_test_id():
    pytest.test_id = str(uuid.uuid4())


@pytest.fixture
def auth_token(client):
    username = f"branch_test_{pytest.test_id}"
    client.post("/auth/register", json={
        "username": username,
        "email": f"{username}@test.com",
        "password": "testpass1234",
    })
    resp = client.post("/auth/login", json={
        "username": username,
        "password": "testpass1234",
    })
    return resp.json()["access_token"]


@pytest.fixture
def headers(auth_token):
    return {"Authorization": f"Bearer {auth_token}"}


# ---------------------------------------------------------------------------
# Cost estimate (no side effects — safe to run always)
# ---------------------------------------------------------------------------

class TestCostEstimate:
    def test_create_is_free(self, client, headers):
        resp = client.post("/api/v1/branches/cost-estimate", json={
            "operation": "create", "model": "gpt-4o-mini",
        }, headers=headers)
        assert resp.status_code == 200
        data = resp.json()
        assert data["estimated_cost"] == 0.0
        assert data["estimated_tokens"] == 0

    def test_diff_has_cost(self, client, headers):
        resp = client.post("/api/v1/branches/cost-estimate", json={
            "operation": "diff", "model": "gpt-4o-mini", "session_count": 100,
        }, headers=headers)
        assert resp.status_code == 200
        assert resp.json()["estimated_tokens"] > 0

    def test_budget_exceeded(self, client, headers):
        resp = client.post("/api/v1/branches/cost-estimate", json={
            "operation": "merge", "model": "gpt-4o-mini",
            "session_count": 1000, "budget_remaining": 0.001,
        }, headers=headers)
        assert resp.status_code == 200
        data = resp.json()
        assert data["exceeds_budget"] is True


# ---------------------------------------------------------------------------
# Branch lifecycle (requires real MatrixOne)
# ---------------------------------------------------------------------------

class TestBranchLifecycle:
    """Create → diff → merge → delete on a real table."""

    BRANCH_TABLE = "branch_api_test_src"
    BRANCH_NAME = "branch_api_test_br"

    @pytest.fixture(autouse=True)
    def _setup_table(self, client, headers):
        """Create a temp source table, yield, then clean up."""
        from api.database import get_db_session
        from sqlalchemy import text

        db = next(get_db_session())
        try:
            db.execute(text(f"DROP TABLE IF EXISTS {self.BRANCH_TABLE}"))
            db.execute(text(
                f"CREATE TABLE {self.BRANCH_TABLE} (id INT PRIMARY KEY, val VARCHAR(50))"
            ))
            db.execute(text(
                f"INSERT INTO {self.BRANCH_TABLE} VALUES (1, 'original')"
            ))
            db.commit()
        except Exception:
            db.rollback()
            pytest.skip("MatrixOne branch DDL not available")

        yield

        # Cleanup
        try:
            db.execute(text(f"DROP TABLE IF EXISTS {self.BRANCH_NAME}"))
            db.execute(text(f"DROP TABLE IF EXISTS {self.BRANCH_TABLE}"))
            db.commit()
        except Exception:
            db.rollback()

    def test_create_branch(self, client, headers):
        resp = client.post("/api/v1/branches", json={
            "name": self.BRANCH_NAME,
            "source": self.BRANCH_TABLE,
        }, headers=headers)
        assert resp.status_code == 201, resp.text
        assert resp.json()["status"] == "created"

    def test_diff_branch(self, client, headers):
        # Create branch first
        client.post("/api/v1/branches", json={
            "name": self.BRANCH_NAME,
            "source": self.BRANCH_TABLE,
        }, headers=headers)

        resp = client.post("/api/v1/branches/diff", json={
            "target": self.BRANCH_NAME,
            "source": self.BRANCH_TABLE,
            "output": "count",
        }, headers=headers)
        assert resp.status_code == 200
        assert "count" in resp.json()

    def test_merge_branch(self, client, headers):
        client.post("/api/v1/branches", json={
            "name": self.BRANCH_NAME,
            "source": self.BRANCH_TABLE,
        }, headers=headers)

        resp = client.post("/api/v1/branches/merge", json={
            "source": self.BRANCH_NAME,
            "target": self.BRANCH_TABLE,
            "on_conflict": "skip",
        }, headers=headers)
        assert resp.status_code == 200
        assert resp.json()["status"] == "merged"

    def test_delete_branch(self, client, headers):
        client.post("/api/v1/branches", json={
            "name": self.BRANCH_NAME,
            "source": self.BRANCH_TABLE,
        }, headers=headers)

        resp = client.request("DELETE", "/api/v1/branches", json={
            "name": self.BRANCH_NAME,
        }, headers=headers)
        assert resp.status_code == 200
        assert resp.json()["status"] == "deleted"


# ---------------------------------------------------------------------------
# Auth
# ---------------------------------------------------------------------------

class TestBranchAuth:
    def test_no_auth_returns_401(self, client):
        resp = client.post("/api/v1/branches", json={
            "name": "x", "source": "y",
        })
        assert resp.status_code == 401

    def test_no_auth_cost_estimate_401(self, client):
        resp = client.post("/api/v1/branches/cost-estimate", json={
            "operation": "create", "model": "gpt-4o-mini",
        })
        assert resp.status_code == 401

    def test_sql_injection_rejected(self, client, headers):
        resp = client.post("/api/v1/branches", json={
            "name": "x; DROP TABLE auth_users; --",
            "source": "y",
        }, headers=headers)
        assert resp.status_code == 422
