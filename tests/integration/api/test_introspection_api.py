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
            db.execute(
                text(
                    "INSERT INTO skills_registry (skill_id, skill_name, version, description, is_active, category) "
                    "VALUES (:id, :n, '1.0.0', :d, 1, :c)"
                ),
                {"id": f"{name}@1.0.0", "n": name, "d": desc, "c": cat},
            )
        for i, suffix in enumerate(["ci", "pr"], 1):
            name = f"{prefix}_{suffix}"
            db.execute(
                text(
                    "INSERT INTO skill_installations "
                    "(installation_id, user_id, skill_name, skill_version, status, installed_at) "
                    "VALUES (:iid, :uid, :n, '1.0.0', 'installed', NOW())"
                ),
                {"iid": str(uuid4()), "uid": user_id, "n": name},
            )
        db.commit()

        def cleanup():
            db.execute(
                text("DELETE FROM skill_installations WHERE user_id = :uid"), {"uid": user_id}
            )
            db.execute(
                text("DELETE FROM skills_registry WHERE skill_name LIKE :pat"),
                {"pat": f"{prefix}%"},
            )
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
        db.execute(
            text(
                "INSERT INTO skills_registry (skill_id, skill_name, version, description, is_active) "
                "VALUES (:id, :n, '1.0.0', 'v1', 1)"
            ),
            {"id": f"{name}@1.0.0", "n": name},
        )
        db.execute(
            text(
                "INSERT INTO skills_registry (skill_id, skill_name, version, description, is_active) "
                "VALUES (:id, :n, '2.0.0', 'v2', 1)"
            ),
            {"id": f"{name}@2.0.0", "n": name},
        )
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
            resp = client.get(
                "/introspection/memory", headers=auth_headers, params={"session_id": s.session_id}
            )
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
        resp = client.get(
            "/introspection/memory", headers=auth_headers, params={"session_id": "nonexistent"}
        )
        assert resp.status_code == 404

    def test_other_users_session_denied(self, client, auth_headers, db):
        s = self._create_session(db, "other_user_id")
        try:
            resp = client.get(
                "/introspection/memory", headers=auth_headers, params={"session_id": s.session_id}
            )
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
            resp = client.get(
                "/introspection/memory", headers=auth_headers, params={"session_id": s.session_id}
            )
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
        """Snapshot with proper budget → verify every health field including new ones."""
        import json
        from api.models.agent import Event as EventModel
        from api.models.context import ContextSnapshot

        s = self._create_session(db, test_user.user_id)
        budget = {
            "history": {"allocated": 1000, "used": 850},
            "code": {"allocated": 500, "used": 100},
        }
        # Insert llm_response so health gets real LLM data
        ev = EventModel(
            event_id=str(uuid4()),
            session_id=s.session_id,
            user_id=test_user.user_id,
            event_type="llm_response",
            content="response",
            causal_chain_id=str(uuid4()),
            token_usage={"prompt": 5000, "completion": 200, "total": 5200},
        )
        db.add(ev)
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
            resp = client.get(
                "/introspection/memory", headers=auth_headers, params={"session_id": s.session_id}
            )
            sem = resp.json()["semantic"]
            assert sem["ctx_snapshots"] == 1
            assert sem["peak_tokens"] == 950
            assert sem["context_managed_tokens"] == 950
            assert sem["last_assembly_ms"] == 42

            # Top-level LLM usage fields
            assert sem["llm_prompt_tokens"] == 5000
            assert sem["llm_completion_tokens"] == 200
            assert sem["llm_total_tokens"] == 5200

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
            assert h["overall_status"] == "high"
            assert h["trend"] == "stable"  # only 1 event
            assert "compaction recommended" in h["recommendation"]

            # llm_usage in health
            assert h["llm_usage"]["prompt"] == 5000
            assert h["llm_usage"]["completion"] == 200
            assert h["llm_usage"]["context_window"] == 128000
            assert h["llm_usage"]["utilization"] == round(5000 / 128000, 3)
        finally:
            db.delete(snap)
            db.delete(ev)
            db.delete(s)
            db.commit()

    def test_semantic_no_llm_data_shows_note(self, client, auth_headers, db, test_user):
        """No llm_response events → llm_usage is null with explanatory note."""
        from api.models.context import ContextSnapshot

        s = self._create_session(db, test_user.user_id)
        snap = ContextSnapshot(
            context_capture_id=str(uuid4()),
            session_id=s.session_id,
            event_id=str(uuid4()),
            token_budget={"history": {"allocated": 1000, "used": 500}},
            total_tokens=500,
            assembly_time_ms=10,
        )
        db.add(snap)
        db.commit()
        try:
            sem = client.get(
                "/introspection/memory", headers=auth_headers, params={"session_id": s.session_id}
            ).json()["semantic"]
            # No top-level llm fields when no data
            assert "llm_prompt_tokens" not in sem
            # Health should have llm_usage=None and note
            h = sem["health"]
            assert h["llm_usage"] is None
            assert "not available" in h["llm_usage_note"]
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
            sem = client.get(
                "/introspection/memory", headers=auth_headers, params={"session_id": s.session_id}
            ).json()["semantic"]
            assert sem["context_managed_tokens"] == 0
            assert sem["last_assembly_ms"] == 0
        finally:
            db.delete(snap)
            db.delete(s)
            db.commit()

    def test_semantic_no_snapshot_keys_absent(self, client, auth_headers, db, test_user):
        """No snapshots → health/context_managed_tokens/last_assembly_ms must not appear."""
        s = self._create_session(db, test_user.user_id)
        try:
            sem = client.get(
                "/introspection/memory", headers=auth_headers, params={"session_id": s.session_id}
            ).json()["semantic"]
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
        assert result == {
            "turns": 0,
            "total_events": 0,
            "tool_intensity": "low",
            "session_depth": "shallow",
        }

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

    def test_skills_endpoint_db_error(self):
        """SQLAlchemyError in /introspection/skills returns empty lists."""
        from unittest.mock import MagicMock
        from sqlalchemy.exc import OperationalError
        from api.routers.introspection import get_introspection_skills

        mock_db = MagicMock()
        mock_db.execute.side_effect = OperationalError("stmt", {}, Exception("down"))
        result = get_introspection_skills(current_user={"user_id": "test"}, db=mock_db)
        assert result == {"installed": [], "cloud": []}


# ============================================================================
# New analysis functions — unit tests
# ============================================================================


class TestAnalysisFunctions:
    """Unit tests for the pure computation functions."""

    def test_context_health_high_utilization(self):
        from api.routers.introspection import _analyze_context_health

        budget = {
            "history": {"allocated": 1000, "used": 900},
            "code": {"allocated": 500, "used": 100},
        }
        result = _analyze_context_health(budget, [5000, 4500, 4000])
        assert result["bottleneck"] == "history"
        assert any(z["status"] == "high" for z in result["zones"])
        assert "compaction recommended" in result["recommendation"]

    def test_context_health_stable(self):
        from api.routers.introspection import _analyze_context_health

        budget = {
            "history": {"allocated": 1000, "used": 300},
            "code": {"allocated": 500, "used": 100},
        }
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

        budget = {
            "history": {"allocated": 800, "used": 0},
            "code": {"allocated": 100, "used": 0},
            "memory": {"allocated": 100, "used": 0},
        }
        result = _zone_balance(budget, "code_gen")
        assert result["balanced"] is False
        assert result["misallocated_zone"] is not None
        assert result["matched_profile"] == "code_gen"

    def test_zone_balance_ok(self):
        from api.routers.introspection import _zone_balance

        budget = {
            "history": {"allocated": 300, "used": 0},
            "code": {"allocated": 400, "used": 0},
            "memory": {"allocated": 150, "used": 0},
        }
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

    # -- budget format compatibility --

    def test_analyze_context_health_nested_format(self):
        """Nested budget {zone: {allocated, used}} — original format."""
        from api.routers.introspection import _analyze_context_health

        budget = {
            "history": {"allocated": 1000, "used": 850},
            "code": {"allocated": 500, "used": 100},
        }
        result = _analyze_context_health(budget, [8000, 7000, 6000])
        zones = {z["name"]: z for z in result["zones"]}
        assert zones["history"]["utilization"] == 0.85
        assert zones["history"]["status"] == "high"
        assert zones["code"]["utilization"] == 0.2
        assert result["bottleneck"] == "history"

    def test_analyze_context_health_flat_format(self):
        """Flat budget {zone: int} — real format from context manager."""
        from api.routers.introspection import _analyze_context_health

        budget = {
            "constraints": 543,
            "identity": 23,
            "memory": 9,
            "project_context": 23,
            "self_model": 372,
        }
        result = _analyze_context_health(budget, [4000, 5000, 6000], llm_prompt_tokens=6000)
        zones = {z["name"] for z in result["zones"]}
        assert "constraints" in zones
        assert "memory" in zones
        # flat format: zones have tokens + share, not utilization
        for z in result["zones"]:
            assert "tokens" in z
            assert "share" in z
            assert "utilization" not in z
        # overall health based on LLM prompt vs context window
        assert result["llm_usage"]["prompt"] == 6000
        assert result["llm_usage"]["utilization"] == round(6000 / 128000, 3)
        assert result["overall_status"] == "ok"  # 6000/128000 = 4.7% → ok

    def test_analyze_context_health_mixed_format(self):
        """Mixed budget (some nested, some flat) — should not crash."""
        from api.routers.introspection import _analyze_context_health

        budget = {"history": {"allocated": 1000, "used": 500}, "memory": 200}
        result = _analyze_context_health(budget, [1200])
        zones = {z["name"]: z for z in result["zones"]}
        assert zones["history"]["utilization"] == 0.5
        assert zones["memory"]["utilization"] == 1.0

    def test_zone_balance_flat_format(self):
        """Flat budget {zone: int} — _zone_balance must not crash."""
        from api.routers.introspection import _zone_balance

        budget = {"history": 500, "code": 300, "memory": 200}
        result = _zone_balance(budget, "code_gen")
        assert isinstance(result["balanced"], bool)
        assert result["matched_profile"] in ("code_gen", "default")

    def test_zone_balance_ignores_unknown_vals(self):
        """Non-int, non-dict values in budget are skipped gracefully."""
        from api.routers.introspection import _zone_balance

        budget = {"history": {"allocated": 500, "used": 0}, "bad_zone": None, "code": 200}
        result = _zone_balance(budget, None)
        assert isinstance(result["balanced"], bool)

    def test_summarize_contents(self):
        """_summarize_contents receives _SnapshotContentRow (named fields, not tuple)."""
        import json
        from api.routers.introspection import _summarize_contents, _SnapshotContentRow

        row = _SnapshotContentRow(
            selected_events=json.dumps(
                [
                    {"event_type": "user_query"},
                    {"event_type": "tool_call"},
                    {"event_type": "user_query"},
                ]
            ),
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
    def _setup(db, user_id, prompt_list=(7000, 8000, 9000)):
        """Insert llm_response events with real token_usage data."""
        from api.models.agent import Session as SessionModel, Event as EventModel
        from datetime import datetime, timezone, timedelta

        s = SessionModel(session_id=str(uuid4()), user_id=user_id, status="active", event_count=0)
        db.add(s)
        db.flush()
        now = datetime.now(timezone.utc)
        events = []
        for i, prompt in enumerate(prompt_list):
            ev = EventModel(
                event_id=str(uuid4()),
                session_id=s.session_id,
                user_id=user_id,
                event_type="llm_response",
                content="response",
                causal_chain_id=str(uuid4()),
                token_usage={"prompt": prompt, "completion": 100, "total": prompt + 100},
                created_at=now + timedelta(seconds=i),
            )
            db.add(ev)
            events.append(ev)
        db.commit()
        return s, events

    def test_trend_growing_all_fields(self, client, auth_headers, db, test_user):
        s, events = self._setup(db, test_user.user_id, [7000, 8000, 9000])
        try:
            resp = client.get(
                "/introspection/context/trend",
                headers=auth_headers,
                params={"session_id": s.session_id},
            )
            assert resp.status_code == 200
            data = resp.json()

            assert data["turns_sampled"] == 3
            assert data["trend"] == "growing"
            # current_tokens is now a dict with real LLM usage
            ct = data["current_tokens"]
            assert ct["prompt"] == 9000
            assert ct["completion"] == 100
            assert ct["total"] == 9100

            # utilization vs context window
            assert "utilization" in data
            assert data["context_window_limit"] == 128000

            # per_turn list
            assert len(data["per_turn"]) == 3

            # Forecast
            fc = data["forecast"]
            assert isinstance(fc["turns_remaining"], int)
            assert fc["turns_remaining"] > 0
            assert fc["growth_rate_per_turn"] == 1000.0

            # Compaction history
            ch = data["compaction_history"]
            assert ch["compactions_detected"] == 0
            assert ch["status"] == "none observed"
        finally:
            for ev in events:
                db.delete(ev)
            db.delete(s)
            db.commit()

    def test_trend_with_compaction_detected(self, client, auth_headers, db, test_user):
        """Token drop > 20% between turns → compaction detected."""
        s, events = self._setup(db, test_user.user_id, [8000, 9000, 5000, 8500])
        try:
            data = client.get(
                "/introspection/context/trend",
                headers=auth_headers,
                params={"session_id": s.session_id},
            ).json()
            ch = data["compaction_history"]
            assert ch["compactions_detected"] >= 1
            assert ch["avg_reduction_pct"] > 0
            assert "effective" in ch["status"] or "weak" in ch["status"]
        finally:
            for ev in events:
                db.delete(ev)
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
            resp = client.get(
                "/introspection/context/trend",
                headers=auth_headers,
                params={"session_id": s.session_id},
            )
            assert resp.status_code == 404
        finally:
            db.delete(s)
            db.commit()

    def test_trend_no_data(self, client, auth_headers, db, test_user):
        from api.models.agent import Session as SessionModel

        s = SessionModel(
            session_id=str(uuid4()), user_id=test_user.user_id, status="active", event_count=0
        )
        db.add(s)
        db.commit()
        try:
            data = client.get(
                "/introspection/context/trend",
                headers=auth_headers,
                params={"session_id": s.session_id},
            ).json()
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
        budget = {
            "history": {"allocated": 1000, "used": 850},
            "code": {"allocated": 500, "used": 100},
        }
        scores = {"e1": 0.9, "e2": 0.15, "e3": 0.8}
        events_data = [
            {"event_type": "user_query", "content": "hello"},
            {"event_type": "tool_call", "content": "search"},
        ]
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
            data = client.get(
                "/introspection/context/snapshot",
                headers=auth_headers,
                params={"session_id": s.session_id},
            ).json()

            # Metadata
            assert data["snapshot_id"] == snap.context_capture_id
            assert data["turn"] == 1
            assert data["total_turns"] == 1
            assert data["task_type"] == "code_gen"
            assert data["context_managed_tokens"] == 950
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
            assert rel["high"] == 2  # 0.9, 0.8
            assert rel["medium"] == 0
            assert rel["low"] == 1  # 0.15
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
            data = client.get(
                "/introspection/context/snapshot",
                headers=auth_headers,
                params={"session_id": s.session_id, "detail": True},
            ).json()

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
            data = client.get(
                "/introspection/context/snapshot",
                headers=auth_headers,
                params={"session_id": s.session_id, "raw": True, "raw_token_budget": 4000},
            ).json()

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
            data = client.get(
                "/introspection/context/snapshot",
                headers=auth_headers,
                params={"session_id": s.session_id, "raw": True, "raw_token_budget": 100},
            ).json()
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

        s = SessionModel(
            session_id=str(uuid4()), user_id=test_user.user_id, status="active", event_count=0
        )
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
            d1 = client.get(
                "/introspection/context/snapshot",
                headers=auth_headers,
                params={"session_id": s.session_id, "turn_index": 1},
            ).json()
            assert d1["turn"] == 1
            assert d1["total_turns"] == 3
            assert d1["context_managed_tokens"] == 5000

            # Turn 3 = newest = 9000 tokens
            d3 = client.get(
                "/introspection/context/snapshot",
                headers=auth_headers,
                params={"session_id": s.session_id, "turn_index": 3},
            ).json()
            assert d3["turn"] == 3
            assert d3["context_managed_tokens"] == 9000

            # Default (no turn_index) = latest
            dl = client.get(
                "/introspection/context/snapshot",
                headers=auth_headers,
                params={"session_id": s.session_id},
            ).json()
            assert dl["context_managed_tokens"] == 9000
        finally:
            for snap in snaps:
                db.delete(snap)
            db.delete(s)
            db.commit()

    def test_snapshot_not_found(self, client, auth_headers, db, test_user):
        from api.models.agent import Session as SessionModel

        s = SessionModel(
            session_id=str(uuid4()), user_id=test_user.user_id, status="active", event_count=0
        )
        db.add(s)
        db.commit()
        try:
            resp = client.get(
                "/introspection/context/snapshot",
                headers=auth_headers,
                params={"session_id": s.session_id},
            )
            assert resp.status_code == 404
        finally:
            db.delete(s)
            db.commit()

    def test_flat_budget_format(self, client, auth_headers, db, test_user):
        """Flat token_budget {zone: int} — real format written by context manager — must not 500."""
        from api.models.agent import Session as SessionModel, Event as EventModel
        from api.models.context import ContextSnapshot

        s = SessionModel(
            session_id=str(uuid4()), user_id=test_user.user_id, status="active", event_count=0
        )
        db.add(s)
        db.flush()
        ev = EventModel(
            event_id=str(uuid4()),
            session_id=s.session_id,
            user_id=test_user.user_id,
            event_type="llm_response",
            content="response",
            causal_chain_id=str(uuid4()),
            token_usage={"prompt": 6000, "completion": 200, "total": 6200},
        )
        db.add(ev)
        snap = ContextSnapshot(
            context_capture_id=str(uuid4()),
            session_id=s.session_id,
            event_id=str(uuid4()),
            token_budget={
                "constraints": 543,
                "identity": 23,
                "memory": 9,
                "project_context": 23,
                "self_model": 372,
            },
            total_tokens=970,
            assembly_time_ms=12,
        )
        db.add(snap)
        db.commit()
        try:
            resp = client.get(
                "/introspection/context/snapshot",
                headers=auth_headers,
                params={"session_id": s.session_id},
            )
            assert resp.status_code == 200, resp.text
            data = resp.json()
            assert "health" in data
            zones = {z["name"] for z in data["health"]["zones"]}
            assert "constraints" in zones
            assert "memory" in zones
            # flat format: zones have tokens + share, not utilization
            for z in data["health"]["zones"]:
                assert "tokens" in z
                assert "share" in z
            # llm_usage from real event
            lu = data["health"]["llm_usage"]
            assert lu["prompt"] == 6000
            assert lu["completion"] == 200
            assert lu["total"] == 6200
            assert lu["context_window"] == 128000
            assert lu["utilization"] == round(6000 / 128000, 3)
            assert data["health"]["overall_status"] == "ok"  # 6000/128000 = 4.7%
            # zone_note removed — unmanaged portion is now an explicit zone in the list
            zones = data["health"]["zones"]
            zone_names = [z["name"] for z in zones]
            assert "conversation_history_and_tools" in zone_names
            unmanaged_zone = next(z for z in zones if z["name"] == "conversation_history_and_tools")
            managed_tokens = sum(v for v in snap.token_budget.values())  # 543+23+9+23+372 = 970
            assert unmanaged_zone["tokens"] == 6000 - managed_tokens
            assert unmanaged_zone["share"] == round((6000 - managed_tokens) / 6000, 3)
            assert "note" not in unmanaged_zone  # no extra fields vs other zones
            # top-level field renamed
            assert data["context_managed_tokens"] == 970
            assert "total_tokens" not in data
            # top-level LLM usage (not buried in health)
            assert data["llm_prompt_tokens"] == 6000
            assert data["llm_completion_tokens"] == 200
            assert data["llm_total_tokens"] == 6200
            assert "zone_balance" in data
        finally:
            db.delete(snap)
            db.delete(ev)
            db.delete(s)
            db.commit()

    def test_no_llm_response_shows_note(self, client, auth_headers, db, test_user):
        """When no llm_response exists, health.llm_usage is null and llm_usage_note explains why."""
        from api.models.agent import Session as SessionModel
        from api.models.context import ContextSnapshot

        s = SessionModel(
            session_id=str(uuid4()), user_id=test_user.user_id, status="active", event_count=0
        )
        db.add(s)
        db.flush()
        snap = ContextSnapshot(
            context_capture_id=str(uuid4()),
            session_id=s.session_id,
            event_id=str(uuid4()),
            token_budget={"constraints": 543, "identity": 23, "memory": 9},
            total_tokens=575,
            assembly_time_ms=10,
        )
        db.add(snap)
        db.commit()
        try:
            resp = client.get(
                "/introspection/context/snapshot",
                headers=auth_headers,
                params={"session_id": s.session_id},
            )
            assert resp.status_code == 200, resp.text
            data = resp.json()
            h = data["health"]
            assert h["llm_usage"] is None
            assert "llm_usage_note" in h
            assert "not available" in h["llm_usage_note"]
            # No top-level LLM fields when no data
            assert "llm_prompt_tokens" not in data
        finally:
            db.delete(snap)
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
        s, snaps = self._setup(
            db, test_user.user_id, [{"a": 0.9, "b": 0.8}, {"a": 0.85, "b": 0.75}]
        )
        try:
            data = client.get(
                "/introspection/context/retrieval_quality",
                headers=auth_headers,
                params={"session_id": s.session_id},
            ).json()
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
            data = client.get(
                "/introspection/context/retrieval_quality",
                headers=auth_headers,
                params={"session_id": s.session_id},
            ).json()
            assert data["overall_quality"] == "poor"
            assert "re-retrieval" in data["recommendation"]
        finally:
            for snap in snaps:
                db.delete(snap)
            db.delete(s)
            db.commit()

    def test_no_data(self, client, auth_headers, db, test_user):
        from api.models.agent import Session as SessionModel

        s = SessionModel(
            session_id=str(uuid4()), user_id=test_user.user_id, status="active", event_count=0
        )
        db.add(s)
        db.commit()
        try:
            data = client.get(
                "/introspection/context/retrieval_quality",
                headers=auth_headers,
                params={"session_id": s.session_id},
            ).json()
            assert data["overall_quality"] == "no_data"
        finally:
            db.delete(s)
            db.commit()

    def test_requires_auth(self, client):
        resp = client.get("/introspection/context/retrieval_quality", params={"session_id": "any"})
        assert resp.status_code in (401, 403)


# ============================================================================
# /introspection/memory/recall
# ============================================================================


class TestMemoryRecallExplain:
    """Test GET /introspection/memory/recall — per-candidate scoring breakdown."""

    def _seed_memories(self, db, user_id: str, session_id: str):
        """Insert test memories with varying confidence and timestamps."""
        from datetime import datetime, timezone, timedelta

        now = datetime.now(timezone.utc)
        memories = []
        for i, (content, conf, age_days) in enumerate(
            [
                ("Python async patterns", 0.9, 1),
                ("Go concurrency model", 0.7, 10),
                ("Rust ownership rules", 0.5, 30),
            ]
        ):
            mid = str(uuid4())
            observed = now - timedelta(days=age_days)
            db.execute(
                text(
                    "INSERT INTO mem_memories "
                    "(memory_id, user_id, memory_type, content, initial_confidence, "
                    " trust_tier, is_active, session_id, source_event_ids, observed_at, created_at) "
                    "VALUES (:mid, :uid, 'semantic', :content, :conf, "
                    " 'T3', 1, :sid, '[]', :obs, :obs)"
                ),
                {
                    "mid": mid,
                    "uid": user_id,
                    "content": content,
                    "conf": conf,
                    "sid": session_id,
                    "obs": observed,
                },
            )
            memories.append(mid)
        db.commit()
        return memories

    def test_recall_returns_ranking_with_scores(self, client, auth_headers, db, test_user):
        """Recall endpoint returns per-candidate 4-dimension score breakdown."""
        from api.models.agent import Session as SessionModel

        session = SessionModel(
            session_id=str(uuid4()),
            user_id=test_user.user_id,
            status="active",
            event_count=0,
        )
        db.add(session)
        db.commit()
        mem_ids = self._seed_memories(db, test_user.user_id, session.session_id)
        try:
            resp = client.get(
                "/introspection/memory/recall",
                headers=auth_headers,
                params={
                    "session_id": session.session_id,
                    "query": "Python async",
                    "task_hint": "code",
                    "limit": 10,
                },
            )
            assert resp.status_code == 200
            data = resp.json()

            # Top-level fields
            assert data["query"] == "Python async"
            assert data["task_hint"] == "code"
            assert data["retrieved_count"] >= 1
            assert data["total_ms"] >= 0

            # Phase stats present
            assert "phases" in data
            assert "keyword" in data["phases"]
            assert "vector" in data["phases"]
            assert "merge" in data["phases"]

            # Ranking with per-candidate scores
            assert "ranking" in data
            ranking = data["ranking"]
            assert len(ranking) >= 1

            for entry in ranking:
                assert "rank" in entry
                assert "memory_id" in entry
                assert entry["memory_id"] in set(mem_ids)  # must be from seeded data
                assert "final_score" in entry
                assert "scores" in entry
                scores = entry["scores"]
                assert "vector" in scores
                assert "keyword" in scores
                assert "temporal" in scores
                assert "confidence" in scores
                # All scores non-negative
                for dim in ("vector", "keyword", "temporal", "confidence"):
                    assert scores[dim] >= 0

            # Ranks are sequential
            ranks = [e["rank"] for e in ranking]
            assert ranks == list(range(1, len(ranking) + 1))

            # Final scores are non-increasing (pairwise, tolerates ties)
            final_scores = [e["final_score"] for e in ranking]
            for a, b in zip(final_scores, final_scores[1:]):
                assert a >= b

        finally:
            db.execute(
                text("DELETE FROM mem_memories WHERE user_id = :uid"), {"uid": test_user.user_id}
            )
            db.delete(session)
            db.commit()

    def test_recall_empty_session(self, client, auth_headers, db, test_user):
        """Recall on session with no memories returns empty ranking."""
        from api.models.agent import Session as SessionModel

        session = SessionModel(
            session_id=str(uuid4()),
            user_id=test_user.user_id,
            status="active",
            event_count=0,
        )
        db.add(session)
        db.commit()
        try:
            resp = client.get(
                "/introspection/memory/recall",
                headers=auth_headers,
                params={"session_id": session.session_id, "query": "anything"},
            )
            assert resp.status_code == 200
            data = resp.json()
            assert data["retrieved_count"] == 0
            # No ranking key when no candidates scored
            assert "ranking" not in data or len(data.get("ranking", [])) == 0
        finally:
            db.delete(session)
            db.commit()

    def test_recall_requires_auth(self, client):
        resp = client.get(
            "/introspection/memory/recall", params={"session_id": "any", "query": "test"}
        )
        assert resp.status_code in (401, 403)

    def test_recall_other_user_denied(self, client, auth_headers, db, test_user):
        """Cannot explain recall for another user's session."""
        from api.models.agent import Session as SessionModel

        other_session = SessionModel(
            session_id=str(uuid4()),
            user_id="other-user-id",
            status="active",
            event_count=0,
        )
        db.add(other_session)
        db.commit()
        try:
            resp = client.get(
                "/introspection/memory/recall",
                headers=auth_headers,
                params={"session_id": other_session.session_id, "query": "test"},
            )
            assert resp.status_code == 404
        finally:
            db.delete(other_session)
            db.commit()
