"""Integration tests for introspection API endpoints."""

import pytest
from uuid import uuid4
from fastapi.testclient import TestClient
from sqlalchemy import text

from api.main import app
from api.database import get_db_session


@pytest.fixture
def client():
    return TestClient(app)


@pytest.fixture
def db():
    session = next(get_db_session())
    yield session
    session.close()


# ============================================================================
# /introspection/skills
# ============================================================================

class TestIntrospectionSkills:
    """Test GET /introspection/skills endpoint."""

    def _seed(self, db, user_id: str):
        """Seed skills + installations for a user. Returns cleanup function.

        Skill names prefixed with ``a_`` so they sort before concurrent
        test data and always land within the cloud-skills cap.
        """
        prefix = f"a_{user_id}"
        for suffix, desc, cat in [
            ("ci", "Check CI status", "devops"),
            ("pr", "List open PRs", "devops"),
            ("sum", "Summarize PR changes", "devops"),
        ]:
            name = f"{prefix}_{suffix}"
            db.execute(text(
                "INSERT INTO skills_registry (skill_id, skill_name, version, description, is_active, category) "
                "VALUES (:id, :n, '1.0.0', :d, 1, :c)"
            ), {"id": f"{name}@1.0.0", "n": name, "d": desc, "c": cat})
        for i, suffix in enumerate(["ci", "pr"], 1):
            name = f"{prefix}_{suffix}"
            db.execute(text(
                "INSERT INTO skill_installations "
                "(installation_id, user_id, skill_name, skill_version, status, installed_at) "
                "VALUES (:iid, :uid, :n, '1.0.0', 'installed', NOW())"
            ), {"iid": str(uuid4()), "uid": user_id, "n": name})
        db.commit()

        def cleanup():
            db.execute(text("DELETE FROM skill_installations WHERE user_id = :uid"), {"uid": user_id})
            db.execute(text("DELETE FROM skills_registry WHERE skill_name LIKE :pat"), {"pat": f"{prefix}%"})
            db.commit()
        return cleanup

    def test_returns_installed_and_cloud(self, client, auth_headers, db, test_user):
        """Installed skills and cloud skills returned separately."""
        cleanup = self._seed(db, test_user.user_id)
        try:
            resp = client.get("/introspection/skills", headers=auth_headers)
            assert resp.status_code == 200
            data = resp.json()
            assert "installed" in data
            assert "cloud" in data

            prefix = f"a_{test_user.user_id}"
            installed_names = {s["name"] for s in data["installed"]}
            assert f"{prefix}_ci" in installed_names
            assert f"{prefix}_pr" in installed_names

            # _sum is NOT installed → should be in cloud, not installed
            assert f"{prefix}_sum" not in installed_names
            cloud_names = {s["name"] for s in data["cloud"]}
            assert f"{prefix}_sum" in cloud_names
        finally:
            cleanup()

    def test_installed_excluded_from_cloud(self, client, auth_headers, db, test_user):
        """Installed skills don't appear in cloud list."""
        cleanup = self._seed(db, test_user.user_id)
        try:
            resp = client.get("/introspection/skills", headers=auth_headers)
            data = resp.json()
            prefix = f"a_{test_user.user_id}"
            cloud_names = {s["name"] for s in data["cloud"]}
            assert f"{prefix}_ci" not in cloud_names
            assert f"{prefix}_pr" not in cloud_names
        finally:
            cleanup()

    def test_installed_has_description(self, client, auth_headers, db, test_user):
        """Installed skills include description from registry."""
        cleanup = self._seed(db, test_user.user_id)
        try:
            resp = client.get("/introspection/skills", headers=auth_headers)
            data = resp.json()
            prefix = f"a_{test_user.user_id}"
            ci = next(s for s in data["installed"] if s["name"] == f"{prefix}_ci")
            assert ci["description"] == "Check CI status"
            assert ci["version"] == "1.0.0"
            assert ci["category"] == "devops"
        finally:
            cleanup()

    def test_multi_version_dedup(self, client, auth_headers, db, test_user):
        """Multiple versions of same skill appear only once in cloud list."""
        prefix = f"a_{test_user.user_id}"
        name = f"{prefix}_multi"
        db.execute(text(
            "INSERT INTO skills_registry (skill_id, skill_name, version, description, is_active) "
            "VALUES (:id, :n, '1.0.0', 'v1', 1)"
        ), {"id": f"{name}@1.0.0", "n": name})
        db.execute(text(
            "INSERT INTO skills_registry (skill_id, skill_name, version, description, is_active) "
            "VALUES (:id, :n, '2.0.0', 'v2', 1)"
        ), {"id": f"{name}@2.0.0", "n": name})
        db.commit()
        try:
            resp = client.get("/introspection/skills", headers=auth_headers)
            data = resp.json()
            matches = [s for s in data["cloud"] if s["name"] == name]
            assert len(matches) == 1
        finally:
            db.execute(text("DELETE FROM skills_registry WHERE skill_name = :n"), {"n": name})
            db.commit()

    def test_empty_when_no_skills(self, client, auth_headers):
        """Returns empty lists when user has no installations and no active skills."""
        resp = client.get("/introspection/skills", headers=auth_headers)
        assert resp.status_code == 200
        data = resp.json()
        assert isinstance(data["installed"], list)
        assert isinstance(data["cloud"], list)

    def test_requires_auth(self, client):
        """Endpoint requires authentication."""
        resp = client.get("/introspection/skills")
        assert resp.status_code in (401, 403)


# ============================================================================
# /introspection/memory
# ============================================================================

class TestIntrospectionMemory:
    """Test GET /introspection/memory endpoint."""

    def _create_session(self, db, user_id: str) -> str:
        """Create a test session owned by user_id."""
        from uuid import uuid4
        sid = str(uuid4())
        db.execute(text(
            "INSERT INTO agent_sessions (session_id, user_id, status, event_count, created_at, last_active_at) "
            "VALUES (:sid, :uid, 'active', 0, NOW(), NOW())"
        ), {"sid": sid, "uid": user_id})
        db.commit()
        return sid

    def test_returns_memory_stats(self, client, auth_headers, db, test_user):
        """Returns episodic, semantic, procedural stats for a valid session."""
        sid = self._create_session(db, test_user.user_id)
        try:
            resp = client.get("/introspection/memory", headers=auth_headers, params={"session_id": sid})
            assert resp.status_code == 200
            data = resp.json()
            assert "episodic" in data
            assert "semantic" in data
            assert "procedural" in data
            assert data["episodic"]["total_events"] == 0
            assert data["semantic"]["ctx_snapshots"] == 0
            assert data["procedural"]["skill_selections"] == 0
        finally:
            db.execute(text("DELETE FROM agent_sessions WHERE session_id = :sid"), {"sid": sid})
            db.commit()

    def test_session_not_found(self, client, auth_headers):
        """Returns 404 for non-existent session."""
        resp = client.get("/introspection/memory", headers=auth_headers, params={"session_id": "nonexistent"})
        assert resp.status_code == 404

    def test_other_users_session_denied(self, client, auth_headers, db):
        """Returns 404 when accessing another user's session (no info leak)."""
        sid = self._create_session(db, "other_user_id")
        try:
            resp = client.get("/introspection/memory", headers=auth_headers, params={"session_id": sid})
            assert resp.status_code == 404
        finally:
            db.execute(text("DELETE FROM agent_sessions WHERE session_id = :sid"), {"sid": sid})
            db.commit()

    def test_requires_auth(self, client):
        """Endpoint requires authentication."""
        resp = client.get("/introspection/memory", params={"session_id": "any"})
        assert resp.status_code in (401, 403)

    def test_session_with_events(self, client, auth_headers, db, test_user):
        """Returns correct counts when session has events."""
        sid = self._create_session(db, test_user.user_id)
        from uuid import uuid4
        # Insert some events
        for etype in ["user_query", "user_query", "tool_call", "llm_response"]:
            db.execute(text(
                "INSERT INTO agent_events "
                "(event_id, session_id, user_id, agent_id, agent_version, event_type, content, causal_chain_id, created_at) "
                "VALUES (:eid, :sid, :uid, 'test-agent', '0.1', :et, 'test', :eid, NOW())"
            ), {"eid": str(uuid4()), "sid": sid, "uid": test_user.user_id, "et": etype})
        db.commit()
        try:
            resp = client.get("/introspection/memory", headers=auth_headers, params={"session_id": sid})
            data = resp.json()
            assert data["episodic"]["total_events"] == 4
            assert data["episodic"]["user_queries"] == 2
            assert data["episodic"]["tool_calls"] == 1
        finally:
            db.execute(text("DELETE FROM agent_events WHERE session_id = :sid"), {"sid": sid})
            db.execute(text("DELETE FROM agent_sessions WHERE session_id = :sid"), {"sid": sid})
            db.commit()


# ============================================================================
# Error paths — defensive code in private helper functions
# ============================================================================

class TestIntrospectionErrorPaths:
    """Test graceful degradation when DB queries fail."""

    def test_episodic_stats_db_error(self):
        """_get_episodic_stats returns zeros on DB error."""
        from unittest.mock import MagicMock
        from api.routers.introspection import _get_episodic_stats
        db = MagicMock()
        db.execute.side_effect = Exception("connection lost")
        result = _get_episodic_stats(db, "any")
        assert result == {"total_events": 0, "user_queries": 0, "tool_calls": 0}

    def test_semantic_stats_db_error(self):
        """_get_semantic_stats returns zeros on DB error."""
        from unittest.mock import MagicMock
        from api.routers.introspection import _get_semantic_stats
        db = MagicMock()
        db.execute.side_effect = Exception("connection lost")
        result = _get_semantic_stats(db, "any")
        assert result == {"ctx_snapshots": 0, "peak_snapshot_tokens": 0}

    def test_procedural_stats_db_error(self):
        """_get_procedural_stats returns zeros on DB error."""
        from unittest.mock import MagicMock
        from api.routers.introspection import _get_procedural_stats
        db = MagicMock()
        db.execute.side_effect = Exception("connection lost")
        result = _get_procedural_stats(db, "any")
        assert result == {"skill_selections": 0, "accuracy_rate": None}

    def test_skills_endpoint_db_error(self, client, auth_headers):
        """SQLAlchemyError in /introspection/skills returns empty lists."""
        from unittest.mock import patch
        from sqlalchemy.exc import OperationalError
        with patch("api.routers.introspection.SessionLocal") as mock_sl:
            mock_db = mock_sl.return_value
            mock_db.execute.side_effect = OperationalError("stmt", {}, Exception("down"))
            resp = client.get("/introspection/skills", headers=auth_headers)
            assert resp.status_code == 200
            assert resp.json() == {"installed": [], "cloud": []}
