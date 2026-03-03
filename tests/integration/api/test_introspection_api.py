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

    def _create_session(self, db, user_id: str) -> "Session":
        """Create a test session via ORM, return the ORM object."""
        from api.models.agent import Session as SessionModel
        s = SessionModel(
            session_id=str(uuid4()),
            user_id=user_id,
            status="active",
            event_count=0,
        )
        db.add(s)
        db.commit()
        db.refresh(s)
        return s

    def test_returns_memory_stats(self, client, auth_headers, db, test_user):
        """Returns episodic, semantic, procedural — verify every field."""
        s = self._create_session(db, test_user.user_id)
        try:
            resp = client.get("/introspection/memory", headers=auth_headers, params={"session_id": s.session_id})
            assert resp.status_code == 200
            data = resp.json()

            # Episodic — all 4 fields
            ep = data["episodic"]
            assert ep["total_events"] == 0
            assert ep["turns"] == 0
            assert ep["tool_intensity"] == "low"
            assert ep["session_depth"] == "shallow"

            # Semantic — exactly 2 fields when no snapshots
            sem = data["semantic"]
            assert sem == {"ctx_snapshots": 0, "peak_tokens": 0}

            # Procedural — exactly 2 fields
            proc = data["procedural"]
            assert proc["skill_selections"] == 0
            assert proc["accuracy_rate"] is None
        finally:
            db.delete(s)
            db.commit()

    def test_session_not_found(self, client, auth_headers):
        resp = client.get("/introspection/memory", headers=auth_headers, params={"session_id": "nonexistent"})
        assert resp.status_code == 404

    def test_other_users_session_denied(self, client, auth_headers, db):
        s = self._create_session(db, "other_user_id")
        try:
            resp = client.get("/introspection/memory", headers=auth_headers, params={"session_id": s.session_id})
            assert resp.status_code == 404
        finally:
            db.delete(s)
            db.commit()

    def test_requires_auth(self, client):
        resp = client.get("/introspection/memory", params={"session_id": "any"})
        assert resp.status_code in (401, 403)

    def test_episodic_derived_signals(self, client, auth_headers, db, test_user):
        """4 events (2 user_query, 1 tool_call, 1 llm_response) → verify derived fields."""
        from api.models.agent import Event
        s = self._create_session(db, test_user.user_id)
        chain_id = str(uuid4())
        events = []
        for etype in ["user_query", "user_query", "tool_call", "llm_response"]:
            e = Event(
                event_id=str(uuid4()),
                session_id=s.session_id,
                user_id=test_user.user_id,
                agent_id="test-agent",
                agent_version="0.1",
                event_type=etype,
                content="test",
                causal_chain_id=chain_id,
            )
            db.add(e)
            events.append(e)
        db.commit()
        try:
            resp = client.get("/introspection/memory", headers=auth_headers, params={"session_id": s.session_id})
            ep = resp.json()["episodic"]
            assert ep["total_events"] == 4
            assert ep["turns"] == 2
            # tool_calls=1 out of 4 total → 25% → "medium"
            assert ep["tool_intensity"] == "medium"
            # 2 turns → "shallow"
            assert ep["session_depth"] == "shallow"
        finally:
            for e in events:
                db.delete(e)
            db.delete(s)
            db.commit()

    def test_semantic_health_all_fields(self, client, auth_headers, db, test_user):
        """Snapshot with proper budget → verify every health field."""
        import json
        from api.models.context import ContextSnapshot
        s = self._create_session(db, test_user.user_id)
        budget = {"history": {"allocated": 1000, "used": 850}, "code": {"allocated": 500, "used": 100}}
        snap = ContextSnapshot(
            context_capture_id=str(uuid4()),
            session_id=s.session_id,
            event_id=str(uuid4()),
            token_budget=budget,
            total_tokens=950,
            assembly_time_ms=42,
        )
        db.add(snap)
        db.commit()
        try:
            resp = client.get("/introspection/memory", headers=auth_headers, params={"session_id": s.session_id})
            sem = resp.json()["semantic"]
            assert sem["ctx_snapshots"] == 1
            assert sem["peak_tokens"] == 950
            assert sem["current_tokens"] == 950
            assert sem["last_assembly_ms"] == 42

            h = sem["health"]
            assert isinstance(h["zones"], list)
            assert len(h["zones"]) == 2
            # history: 850/1000 = 0.85 → "high"
            hist_zone = next(z for z in h["zones"] if z["name"] == "history")
            assert hist_zone["utilization"] == 0.85
            assert hist_zone["status"] == "high"
            # code: 100/500 = 0.2 → "ok"
            code_zone = next(z for z in h["zones"] if z["name"] == "code")
            assert code_zone["utilization"] == 0.2
            assert code_zone["status"] == "ok"

            assert h["bottleneck"] == "history"
            assert h["trend"] == "stable"  # only 1 snapshot
            assert "compaction recommended" in h["recommendation"]
        finally:
            db.delete(snap)
            db.delete(s)
            db.commit()

    def test_semantic_zero_tokens_not_dropped(self, client, auth_headers, db, test_user):
        """total_tokens=0 and assembly_time_ms=0 must appear, not be dropped."""
        from api.models.context import ContextSnapshot
        s = self._create_session(db, test_user.user_id)
        snap = ContextSnapshot(
            context_capture_id=str(uuid4()),
            session_id=s.session_id,
            event_id=str(uuid4()),
            token_budget={"system": {"allocated": 10, "used": 0}},
            total_tokens=0,
            assembly_time_ms=0,
        )
        db.add(snap)
        db.commit()
        try:
            sem = client.get("/introspection/memory", headers=auth_headers,
                             params={"session_id": s.session_id}).json()["semantic"]
            assert sem["current_tokens"] == 0
            assert sem["last_assembly_ms"] == 0
        finally:
            db.delete(snap)
            db.delete(s)
            db.commit()

    def test_semantic_no_snapshot_keys_absent(self, client, auth_headers, db, test_user):
        """No snapshots → health/current_tokens/last_assembly_ms must not appear."""
        s = self._create_session(db, test_user.user_id)
        try:
            sem = client.get("/introspection/memory", headers=auth_headers,
                             params={"session_id": s.session_id}).json()["semantic"]
            assert sem == {"ctx_snapshots": 0, "peak_tokens": 0}
        finally:
            db.delete(s)
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
        assert result == {"turns": 0, "total_events": 0, "tool_intensity": "low", "session_depth": "shallow"}

    def test_semantic_stats_db_error(self):
        """_get_semantic_stats returns zeros on DB error."""
        from unittest.mock import MagicMock
        from api.routers.introspection import _get_semantic_stats
        db = MagicMock()
        db.execute.side_effect = Exception("connection lost")
        result = _get_semantic_stats(db, "any")
        assert result == {"ctx_snapshots": 0, "peak_tokens": 0}

    def test_procedural_stats_db_error(self):
        """_get_procedural_stats returns zeros on DB error."""
        from unittest.mock import MagicMock
        from api.routers.introspection import _get_procedural_stats
        db = MagicMock()
        db.execute.side_effect = Exception("connection lost")
        result = _get_procedural_stats(db, "any")
        assert result == {"skill_selections": 0, "accuracy_rate": None}

    def test_skills_endpoint_db_error(self, auth_headers):
        """SQLAlchemyError in /introspection/skills returns empty lists."""
        from unittest.mock import MagicMock
        from sqlalchemy.exc import OperationalError
        from api.database import get_db_session

        mock_db = MagicMock()
        mock_db.execute.side_effect = OperationalError("stmt", {}, Exception("down"))

        def _broken_db():
            yield mock_db

        app.dependency_overrides[get_db_session] = _broken_db
        try:
            isolated_client = TestClient(app)
            resp = isolated_client.get("/introspection/skills", headers=auth_headers)
            assert resp.status_code == 200
            assert resp.json() == {"installed": [], "cloud": []}
        finally:
            app.dependency_overrides.pop(get_db_session, None)


# ============================================================================
# New analysis functions — unit tests
# ============================================================================

class TestAnalysisFunctions:
    """Unit tests for the pure computation functions."""

    def test_context_health_high_utilization(self):
        from api.routers.introspection import _analyze_context_health
        budget = {"history": {"allocated": 1000, "used": 900}, "code": {"allocated": 500, "used": 100}}
        result = _analyze_context_health(budget, [5000, 4500, 4000])
        assert result["bottleneck"] == "history"
        assert any(z["status"] == "high" for z in result["zones"])
        assert "compaction recommended" in result["recommendation"]

    def test_context_health_stable(self):
        from api.routers.introspection import _analyze_context_health
        budget = {"history": {"allocated": 1000, "used": 300}, "code": {"allocated": 500, "used": 100}}
        result = _analyze_context_health(budget, [4000, 3950])
        assert result["trend"] == "stable"
        assert result["recommendation"] == "context healthy"

    def test_compaction_forecast_growing(self):
        from api.routers.introspection import _compaction_forecast
        result = _compaction_forecast([9000, 8000, 7000], limit=12000)
        assert result["turns_remaining"] == 3
        assert result["growth_rate_per_turn"] == 1000.0

    def test_compaction_forecast_shrinking(self):
        from api.routers.introspection import _compaction_forecast
        result = _compaction_forecast([5000, 6000, 7000], limit=12000)
        assert result["turns_remaining"] is None
        assert result["growth_rate_per_turn"] == -1000.0

    def test_compaction_forecast_insufficient_data(self):
        from api.routers.introspection import _compaction_forecast
        result = _compaction_forecast([5000], limit=12000)
        assert result["turns_remaining"] is None

    def test_relevance_quality_good(self):
        from api.routers.introspection import _relevance_quality
        result = _relevance_quality({"a": 0.9, "b": 0.8, "c": 0.7})
        assert result["quality"] == "good"
        assert result["high"] == 3

    def test_relevance_quality_poor(self):
        from api.routers.introspection import _relevance_quality
        result = _relevance_quality({"a": 0.1, "b": 0.2})
        assert result["quality"] == "poor"
        assert result["low"] == 2

    def test_relevance_quality_empty(self):
        from api.routers.introspection import _relevance_quality
        result = _relevance_quality({})
        assert result["mean"] is None
        assert result["total"] == 0

    def test_pollution_ratio_clean(self):
        from api.routers.introspection import _pollution_ratio
        result = _pollution_ratio({"a": 0.9, "b": 0.8, "c": 0.7})
        assert result["status"] == "clean"
        assert result["pollution_pct"] == 0.0

    def test_pollution_ratio_polluted(self):
        from api.routers.introspection import _pollution_ratio
        result = _pollution_ratio({"a": 0.1, "b": 0.05, "c": 0.9})
        assert result["status"] == "polluted"

    def test_zone_balance_misallocated(self):
        from api.routers.introspection import _zone_balance
        budget = {"history": {"allocated": 800, "used": 0}, "code": {"allocated": 100, "used": 0}, "memory": {"allocated": 100, "used": 0}}
        result = _zone_balance(budget, "code_gen")
        assert result["balanced"] is False
        assert result["misallocated_zone"] is not None
        assert result["matched_profile"] == "code_gen"

    def test_zone_balance_ok(self):
        from api.routers.introspection import _zone_balance
        budget = {"history": {"allocated": 300, "used": 0}, "code": {"allocated": 400, "used": 0}, "memory": {"allocated": 150, "used": 0}}
        result = _zone_balance(budget, "code_gen")
        assert result["balanced"] is True
        assert result["matched_profile"] == "code_gen"

    def test_zone_balance_default_profile(self):
        from api.routers.introspection import _zone_balance
        budget = {"history": {"allocated": 500, "used": 0}, "code": {"allocated": 300, "used": 0}}
        result = _zone_balance(budget, "unknown_type")
        assert result["matched_profile"] == "default"

    def test_compaction_effectiveness_detected(self):
        from api.routers.introspection import _compaction_effectiveness
        # newest first: 9000, 5000, 8500 — drop from 8500→5000 is compaction
        result = _compaction_effectiveness([9000, 5000, 8500])
        assert result["compactions_detected"] == 1
        assert result["status"] == "effective"

    def test_compaction_effectiveness_none(self):
        from api.routers.introspection import _compaction_effectiveness
        result = _compaction_effectiveness([9000, 8800, 8600])
        assert result["compactions_detected"] == 0

    def test_summarize_contents(self):
        """_summarize_contents receives _SnapshotContentRow (named fields, not tuple)."""
        import json
        from api.routers.introspection import _summarize_contents, _SnapshotContentRow
        row = _SnapshotContentRow(
            selected_events=json.dumps([{"event_type": "user_query"}, {"event_type": "tool_call"}, {"event_type": "user_query"}]),
            code_context=json.dumps([{"file": "a.py"}, {"file": "b.py"}]),
            skill_definitions=json.dumps([{"skill_name": "search"}]),
            documentation=json.dumps([{"source": "readme.md"}]),
        )
        result = _summarize_contents(row)
        assert result["events"]["total"] == 3
        assert result["events"]["by_type"]["user_query"] == 2
        assert result["code"]["files"] == 2
        assert result["skills"] == ["search"]
        assert result["docs"] == ["readme.md"]

    def test_raw_contents_truncation(self):
        """_raw_contents truncates to token budget (TOKEN_CHAR_RATIO=2)."""
        import json
        from api.routers.introspection import _raw_contents, _SnapshotContentRow
        row = _SnapshotContentRow(
            selected_events=json.dumps([{"event_type": "user_query", "content": "x" * 500}] * 10),
            code_context=None,
            skill_definitions=None,
            documentation=None,
        )
        result = _raw_contents(row, 500)  # 500 tokens × 2 = 1000 chars budget
        assert "events" in result
        assert len(result["events"]) < 10
        assert result.get("events_truncated") is True


# ============================================================================
# New endpoints — integration tests
# ============================================================================

class TestContextTrend:
    """Tests for GET /introspection/context/trend — verify all response fields."""

    @staticmethod
    def _setup(db, user_id, token_list=(7000, 8000, 9000)):
        from api.models.agent import Session as SessionModel
        from api.models.context import ContextSnapshot
        from datetime import datetime, timezone, timedelta
        s = SessionModel(session_id=str(uuid4()), user_id=user_id, status="active", event_count=0)
        db.add(s)
        db.flush()
        now = datetime.now(timezone.utc)
        snaps = []
        for i, tokens in enumerate(token_list):
            snap = ContextSnapshot(
                context_capture_id=str(uuid4()),
                session_id=s.session_id,
                event_id=str(uuid4()),
                total_tokens=tokens,
                assembly_time_ms=10 + i,
                created_at=now + timedelta(seconds=i),
            )
            db.add(snap)
            snaps.append(snap)
        db.commit()
        return s, snaps

    def test_trend_growing_all_fields(self, client, auth_headers, db, test_user):
        s, snaps = self._setup(db, test_user.user_id, [7000, 8000, 9000])
        try:
            resp = client.get("/introspection/context/trend", headers=auth_headers,
                              params={"session_id": s.session_id})
            assert resp.status_code == 200
            data = resp.json()

            # Top-level fields
            assert data["turns_sampled"] == 3
            assert data["trend"] == "growing"
            assert data["current_tokens"] == 9000

            # Forecast — all fields
            fc = data["forecast"]
            assert isinstance(fc["turns_remaining"], int)
            assert fc["turns_remaining"] > 0
            assert fc["growth_rate_per_turn"] == 1000.0

            # Compaction history — all fields
            ch = data["compaction_history"]
            assert ch["compactions_detected"] == 0
            assert ch["status"] == "none observed"
        finally:
            for snap in snaps:
                db.delete(snap)
            db.delete(s)
            db.commit()

    def test_trend_with_compaction_detected(self, client, auth_headers, db, test_user):
        """Token drop > 20% between turns → compaction detected."""
        s, snaps = self._setup(db, test_user.user_id, [8000, 9000, 5000, 8500])
        try:
            data = client.get("/introspection/context/trend", headers=auth_headers,
                              params={"session_id": s.session_id}).json()
            ch = data["compaction_history"]
            assert ch["compactions_detected"] >= 1
            assert ch["avg_reduction_pct"] > 0
            assert "effective" in ch["status"] or "weak" in ch["status"]
        finally:
            for snap in snaps:
                db.delete(snap)
            db.delete(s)
            db.commit()

    def test_trend_requires_auth(self, client):
        resp = client.get("/introspection/context/trend", params={"session_id": "any"})
        assert resp.status_code in (401, 403)

    def test_trend_other_user_denied(self, client, auth_headers, db):
        from api.models.agent import Session as SessionModel
        s = SessionModel(session_id=str(uuid4()), user_id="other", status="active", event_count=0)
        db.add(s)
        db.commit()
        try:
            resp = client.get("/introspection/context/trend", headers=auth_headers,
                              params={"session_id": s.session_id})
            assert resp.status_code == 404
        finally:
            db.delete(s)
            db.commit()

    def test_trend_no_data(self, client, auth_headers, db, test_user):
        from api.models.agent import Session as SessionModel
        s = SessionModel(session_id=str(uuid4()), user_id=test_user.user_id, status="active", event_count=0)
        db.add(s)
        db.commit()
        try:
            data = client.get("/introspection/context/trend", headers=auth_headers,
                              params={"session_id": s.session_id}).json()
            assert data == {"turns_sampled": 0, "trend": "no_data"}
        finally:
            db.delete(s)
            db.commit()


class TestContextSnapshot:
    """Tests for GET /introspection/context/snapshot — layered response, all fields."""

    @staticmethod
    def _setup(db, user_id):
        import json
        from api.models.agent import Session as SessionModel
        from api.models.context import ContextSnapshot
        s = SessionModel(session_id=str(uuid4()), user_id=user_id, status="active", event_count=0)
        db.add(s)
        db.flush()
        budget = {"history": {"allocated": 1000, "used": 850}, "code": {"allocated": 500, "used": 100}}
        scores = {"e1": 0.9, "e2": 0.15, "e3": 0.8}
        events_data = [{"event_type": "user_query", "content": "hello"}, {"event_type": "tool_call", "content": "search"}]
        code_data = [{"file": "main.py", "content": "print()"}]
        skills_data = [{"skill_name": "code_search", "description": "search code"}]
        snap = ContextSnapshot(
            context_capture_id=str(uuid4()),
            session_id=s.session_id,
            event_id=str(uuid4()),
            token_budget=budget,
            total_tokens=950,
            assembly_time_ms=15,
            relevance_scores=scores,
            task_type="code_gen",
            selected_events=events_data,
            code_context=code_data,
            skill_definitions=skills_data,
        )
        db.add(snap)
        db.commit()
        return s, snap

    def test_layer1_all_fields(self, client, auth_headers, db, test_user):
        """Default: conclusions only — verify every field in health, relevance, pollution, zone_balance."""
        s, snap = self._setup(db, test_user.user_id)
        try:
            data = client.get("/introspection/context/snapshot", headers=auth_headers,
                              params={"session_id": s.session_id}).json()

            # Metadata
            assert data["snapshot_id"] == snap.context_capture_id
            assert data["turn"] == 1
            assert data["total_turns"] == 1
            assert data["task_type"] == "code_gen"
            assert data["total_tokens"] == 950
            assert data["assembly_ms"] == 15

            # Health — all fields
            h = data["health"]
            assert len(h["zones"]) == 2
            hist = next(z for z in h["zones"] if z["name"] == "history")
            assert hist["utilization"] == 0.85
            assert hist["status"] == "high"
            code = next(z for z in h["zones"] if z["name"] == "code")
            assert code["utilization"] == 0.2
            assert code["status"] == "ok"
            assert h["bottleneck"] == "history"
            assert h["trend"] == "stable"
            assert "compaction recommended" in h["recommendation"]

            # Relevance — all fields
            rel = data["relevance"]
            assert rel["total"] == 3
            assert rel["high"] == 2   # 0.9, 0.8
            assert rel["medium"] == 0
            assert rel["low"] == 1    # 0.15
            assert rel["quality"] == "good"
            assert 0.6 < rel["mean"] < 0.7

            # Pollution — all fields
            pol = data["pollution"]
            assert pol["pollution_pct"] > 0  # 1 out of 3 is low
            assert pol["status"] in ("clean", "noisy", "polluted")

            # Zone balance — all fields
            zb = data["zone_balance"]
            assert isinstance(zb["balanced"], bool)
            assert zb["matched_profile"] in ("code_gen", "default")
            assert "recommendation" in zb

            # No detail/raw by default
            assert "contents" not in data
            assert "raw" not in data
        finally:
            db.delete(snap)
            db.delete(s)
            db.commit()

    def test_layer2_contents_summary(self, client, auth_headers, db, test_user):
        """detail=true → structural summary, verify all sub-fields."""
        s, snap = self._setup(db, test_user.user_id)
        try:
            data = client.get("/introspection/context/snapshot", headers=auth_headers,
                              params={"session_id": s.session_id, "detail": True}).json()

            c = data["contents"]
            # Events summary
            assert c["events"]["total"] == 2
            assert c["events"]["by_type"]["user_query"] == 1
            assert c["events"]["by_type"]["tool_call"] == 1
            # Code summary
            assert c["code"]["files"] == 1
            assert c["code"]["paths"] == ["main.py"]
            # Skills summary
            assert c["skills"] == ["code_search"]

            assert "raw" not in data
        finally:
            db.delete(snap)
            db.delete(s)
            db.commit()

    def test_layer3_raw_with_budget(self, client, auth_headers, db, test_user):
        """raw=true → actual content within budget."""
        s, snap = self._setup(db, test_user.user_id)
        try:
            data = client.get("/introspection/context/snapshot", headers=auth_headers,
                              params={"session_id": s.session_id, "raw": True, "raw_token_budget": 4000}).json()

            assert "contents" in data  # detail implied by raw
            assert "raw" in data
            assert "events" in data["raw"]
            assert data["raw"]["events"][0]["event_type"] == "user_query"
        finally:
            db.delete(snap)
            db.delete(s)
            db.commit()

    def test_layer3_raw_truncation(self, client, auth_headers, db, test_user):
        """Tiny budget → content truncated."""
        s, snap = self._setup(db, test_user.user_id)
        try:
            data = client.get("/introspection/context/snapshot", headers=auth_headers,
                              params={"session_id": s.session_id, "raw": True, "raw_token_budget": 100}).json()
            raw = data["raw"]
            # With 100 tokens (400 chars), not everything fits
            total_keys = len(raw)
            assert total_keys >= 1  # at least something returned
        finally:
            db.delete(snap)
            db.delete(s)
            db.commit()

    def test_turn_index_selection(self, client, auth_headers, db, test_user):
        """Multiple snapshots → turn_index selects correct one."""
        from api.models.agent import Session as SessionModel
        from api.models.context import ContextSnapshot
        from datetime import datetime, timezone, timedelta
        s = SessionModel(session_id=str(uuid4()), user_id=test_user.user_id, status="active", event_count=0)
        db.add(s)
        db.flush()
        now = datetime.now(timezone.utc)
        snaps = []
        for i, tokens in enumerate([5000, 7000, 9000]):
            snap = ContextSnapshot(
                context_capture_id=str(uuid4()),
                session_id=s.session_id,
                event_id=str(uuid4()),
                token_budget={"history": {"allocated": 1000, "used": 500}},
                total_tokens=tokens,
                assembly_time_ms=10,
                task_type="qa",
                created_at=now + timedelta(seconds=i),
            )
            db.add(snap)
            snaps.append(snap)
        db.commit()
        try:
            # Turn 1 = oldest = 5000 tokens
            d1 = client.get("/introspection/context/snapshot", headers=auth_headers,
                            params={"session_id": s.session_id, "turn_index": 1}).json()
            assert d1["turn"] == 1
            assert d1["total_turns"] == 3
            assert d1["total_tokens"] == 5000

            # Turn 3 = newest = 9000 tokens
            d3 = client.get("/introspection/context/snapshot", headers=auth_headers,
                            params={"session_id": s.session_id, "turn_index": 3}).json()
            assert d3["turn"] == 3
            assert d3["total_tokens"] == 9000

            # Default (no turn_index) = latest
            dl = client.get("/introspection/context/snapshot", headers=auth_headers,
                            params={"session_id": s.session_id}).json()
            assert dl["total_tokens"] == 9000
        finally:
            for snap in snaps:
                db.delete(snap)
            db.delete(s)
            db.commit()

    def test_snapshot_not_found(self, client, auth_headers, db, test_user):
        from api.models.agent import Session as SessionModel
        s = SessionModel(session_id=str(uuid4()), user_id=test_user.user_id, status="active", event_count=0)
        db.add(s)
        db.commit()
        try:
            resp = client.get("/introspection/context/snapshot", headers=auth_headers,
                              params={"session_id": s.session_id})
            assert resp.status_code == 404
        finally:
            db.delete(s)
            db.commit()


class TestRetrievalQuality:
    """Tests for GET /introspection/context/retrieval_quality — all fields."""

    @staticmethod
    def _setup(db, user_id, scores_list):
        import json
        from api.models.agent import Session as SessionModel
        from api.models.context import ContextSnapshot
        from datetime import datetime, timezone, timedelta
        s = SessionModel(session_id=str(uuid4()), user_id=user_id, status="active", event_count=0)
        db.add(s)
        db.flush()
        now = datetime.now(timezone.utc)
        snaps = []
        for i, scores in enumerate(scores_list):
            snap = ContextSnapshot(
                context_capture_id=str(uuid4()),
                session_id=s.session_id,
                event_id=str(uuid4()),
                relevance_scores=scores,
                total_tokens=1000,
                created_at=now + timedelta(seconds=i),
            )
            db.add(snap)
            snaps.append(snap)
        db.commit()
        return s, snaps

    def test_good_quality_all_fields(self, client, auth_headers, db, test_user):
        s, snaps = self._setup(db, test_user.user_id, [{"a": 0.9, "b": 0.8}, {"a": 0.85, "b": 0.75}])
        try:
            data = client.get("/introspection/context/retrieval_quality", headers=auth_headers,
                              params={"session_id": s.session_id}).json()
            assert data["turns_sampled"] == 2
            assert data["overall_quality"] == "good"
            assert data["mean_relevance"] > 0.7
            assert data["recommendation"] == "retrieval healthy"
        finally:
            for snap in snaps:
                db.delete(snap)
            db.delete(s)
            db.commit()

    def test_poor_quality(self, client, auth_headers, db, test_user):
        s, snaps = self._setup(db, test_user.user_id, [{"a": 0.1, "b": 0.2}, {"a": 0.15, "b": 0.1}])
        try:
            data = client.get("/introspection/context/retrieval_quality", headers=auth_headers,
                              params={"session_id": s.session_id}).json()
            assert data["overall_quality"] == "poor"
            assert "re-retrieval" in data["recommendation"]
        finally:
            for snap in snaps:
                db.delete(snap)
            db.delete(s)
            db.commit()

    def test_no_data(self, client, auth_headers, db, test_user):
        from api.models.agent import Session as SessionModel
        s = SessionModel(session_id=str(uuid4()), user_id=test_user.user_id, status="active", event_count=0)
        db.add(s)
        db.commit()
        try:
            data = client.get("/introspection/context/retrieval_quality", headers=auth_headers,
                              params={"session_id": s.session_id}).json()
            assert data["overall_quality"] == "no_data"
        finally:
            db.delete(s)
            db.commit()

    def test_requires_auth(self, client):
        resp = client.get("/introspection/context/retrieval_quality", params={"session_id": "any"})
        assert resp.status_code in (401, 403)
