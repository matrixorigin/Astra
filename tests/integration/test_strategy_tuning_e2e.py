"""Integration tests for Phase 4: Strategy Tuning & A/B Comparison.

Tests:
- Strategy params validation (Pydantic schemas)
- Param override propagation (experiment → strategy)
- A/B comparison of two experiments
- Params persisted correctly in DB
"""

import json
import os
import uuid

import pytest
from sqlalchemy import text

os.environ.setdefault("MATRIXONE_DATABASE", "test_dev_agent_v3")
_TEST_DB = os.environ.get("MATRIXONE_DATABASE", "test_dev_agent_v3")

from api.database import SessionLocal  # noqa: E402
from core.memory.experiment import MemoryExperimentManager  # noqa: E402
from core.memory.strategy.params import (  # noqa: E402
    ActivationV1Params,
    InvalidStrategyParamsError,
    VectorV1Params,
    get_default_params,
    validate_strategy_params,
)

_TEST_DB = os.environ.get("MATRIXONE_DATABASE", "test_dev_agent_v3")


@pytest.fixture()
def db_factory():
    return SessionLocal


@pytest.fixture()
def mgr(db_factory):
    return MemoryExperimentManager(db_factory, source_db=_TEST_DB)


@pytest.fixture(autouse=True)
def _cleanup(db_factory):
    """Clean up experiment records and branch DBs after each test."""
    yield
    with db_factory() as db:
        rows = db.execute(
            text(
                "SELECT branch_db, base_snapshot FROM mem_experiments "
                "WHERE user_id LIKE 'test_p4_%'"
            )
        ).fetchall()
        branch_dbs = [r.branch_db for r in rows]
        snap_names = [r.base_snapshot for r in rows if r.base_snapshot]
        db.execute(text("DELETE FROM mem_experiments WHERE user_id LIKE 'test_p4_%'"))
        db.commit()

    for bdb in branch_dbs:
        try:
            with db_factory() as db:
                db.commit()
                db.execute(text(f"DROP DATABASE IF EXISTS `{bdb}`"))
                db.commit()
        except Exception:
            pass

    for snap in snap_names:
        try:
            with db_factory() as db:
                db.commit()
                db.execute(text(f"DROP SNAPSHOT IF EXISTS {snap}"))
                db.commit()
        except Exception:
            pass


# ── Params Validation (Unit-level, no DB) ─────────────────────────────


class TestStrategyParamsValidation:
    def test_vector_v1_defaults(self):
        """VectorV1Params has correct defaults."""
        p = VectorV1Params()
        assert p.semantic_weight == 0.4
        assert p.temporal_weight == 0.3
        assert p.confidence_weight == 0.2
        assert p.importance_weight == 0.1

    def test_activation_v1_defaults(self):
        """ActivationV1Params has correct defaults."""
        p = ActivationV1Params()
        assert p.spreading_factor == 0.8
        assert p.num_iterations == 3
        assert p.inhibition_beta == 0.15
        assert p.sigmoid_theta == 0.1
        assert p.min_graph_nodes == 50

    def test_validate_valid_params(self):
        """Valid params pass validation and get defaults filled."""
        result = validate_strategy_params(
            "vector:v1", {"semantic_weight": 0.6},
        )
        assert result is not None
        assert result["semantic_weight"] == 0.6
        # Other fields get defaults
        assert result["temporal_weight"] == 0.3

    def test_validate_none_params(self):
        """None params are always valid."""
        assert validate_strategy_params("vector:v1", None) is None

    def test_validate_invalid_params_raises(self):
        """Out-of-range params raise InvalidStrategyParamsError."""
        with pytest.raises(InvalidStrategyParamsError):
            validate_strategy_params("vector:v1", {"semantic_weight": 2.0})

    def test_validate_invalid_type_raises(self):
        """Wrong type raises InvalidStrategyParamsError."""
        with pytest.raises(InvalidStrategyParamsError):
            validate_strategy_params("activation:v1", {"num_iterations": -1})

    def test_validate_unknown_strategy_passes_through(self):
        """Unknown strategy key passes params through without validation."""
        params = {"custom_param": 42}
        result = validate_strategy_params("custom:v1", params)
        assert result == params

    def test_get_default_params_known(self):
        """get_default_params returns defaults for known strategies."""
        defaults = get_default_params("vector:v1")
        assert defaults is not None
        assert defaults["semantic_weight"] == 0.4

    def test_get_default_params_unknown(self):
        """get_default_params returns None for unknown strategies."""
        assert get_default_params("unknown:v1") is None


# ── Params Persistence in Experiments ─────────────────────────────────


class TestExperimentParamsValidation:
    def test_create_validates_params(self, mgr):
        """create() validates params against strategy schema."""
        user_id = f"test_p4_{uuid.uuid4().hex[:8]}"
        with pytest.raises(InvalidStrategyParamsError):
            mgr.create(
                user_id, "bad-params",
                strategy_key="vector:v1",
                params={"semantic_weight": 5.0},  # out of range
            )

    def test_create_stores_validated_params(self, mgr, db_factory):
        """create() stores validated params (with defaults filled) in DB."""
        user_id = f"test_p4_{uuid.uuid4().hex[:8]}"
        info = mgr.create(
            user_id, "tuning-exp",
            strategy_key="activation:v1",
            params={"spreading_factor": 0.9},
        )

        # Verify params_json in DB has all fields (defaults filled)
        with db_factory() as db:
            row = db.execute(
                text(
                    "SELECT strategy_key, params_json "
                    "FROM mem_experiments WHERE experiment_id = :eid"
                ),
                {"eid": info.experiment_id},
            ).fetchone()
            m = row._mapping
            assert m["strategy_key"] == "activation:v1"
            pj = m["params_json"]
            if isinstance(pj, str):
                pj = json.loads(pj)
            assert pj["spreading_factor"] == 0.9
            # Defaults filled in
            assert pj["num_iterations"] == 3
            assert pj["inhibition_beta"] == 0.15
            assert pj["sigmoid_theta"] == 0.1
            assert pj["min_graph_nodes"] == 50

    def test_create_no_params_stores_none(self, mgr, db_factory):
        """create() without params stores NULL in DB."""
        user_id = f"test_p4_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "no-params-exp")

        with db_factory() as db:
            row = db.execute(
                text(
                    "SELECT params_json FROM mem_experiments "
                    "WHERE experiment_id = :eid"
                ),
                {"eid": info.experiment_id},
            ).fetchone()
            assert row._mapping["params_json"] is None

    def test_get_returns_validated_params(self, mgr):
        """get() returns the validated params_json."""
        user_id = f"test_p4_{uuid.uuid4().hex[:8]}"
        info = mgr.create(
            user_id, "get-params",
            strategy_key="vector:v1",
            params={"semantic_weight": 0.7, "temporal_weight": 0.1},
        )

        fetched = mgr.get(info.experiment_id)
        assert fetched is not None
        assert fetched.params_json is not None
        assert fetched.params_json["semantic_weight"] == 0.7
        assert fetched.params_json["temporal_weight"] == 0.1
        # Defaults filled
        assert fetched.params_json["confidence_weight"] == 0.2
        assert fetched.params_json["importance_weight"] == 0.1

    def test_create_all_vector_params_overridden(self, mgr, db_factory):
        """Override every vector:v1 param — all stored, no defaults."""
        user_id = f"test_p4_{uuid.uuid4().hex[:8]}"
        full_params = {
            "semantic_weight": 0.1,
            "temporal_weight": 0.2,
            "confidence_weight": 0.3,
            "importance_weight": 0.4,
        }
        info = mgr.create(
            user_id, "full-override",
            strategy_key="vector:v1",
            params=full_params,
        )
        with db_factory() as db:
            row = db.execute(
                text("SELECT params_json FROM mem_experiments WHERE experiment_id = :eid"),
                {"eid": info.experiment_id},
            ).fetchone()
            pj = row._mapping["params_json"]
            if isinstance(pj, str):
                pj = json.loads(pj)
            assert pj == full_params

    def test_create_boundary_values(self, mgr):
        """Boundary values (0.0, 1.0) are accepted."""
        user_id = f"test_p4_{uuid.uuid4().hex[:8]}"
        info = mgr.create(
            user_id, "boundary",
            strategy_key="vector:v1",
            params={"semantic_weight": 0.0, "temporal_weight": 1.0},
        )
        assert info.params_json["semantic_weight"] == 0.0
        assert info.params_json["temporal_weight"] == 1.0

    def test_create_multiple_invalid_fields(self, mgr):
        """Multiple invalid fields still raise."""
        user_id = f"test_p4_{uuid.uuid4().hex[:8]}"
        with pytest.raises(InvalidStrategyParamsError):
            mgr.create(
                user_id, "multi-bad",
                strategy_key="activation:v1",
                params={"spreading_factor": -1.0, "num_iterations": 0},
            )

    def test_create_extra_unknown_field_rejected(self, mgr):
        """Extra fields not in schema are rejected by Pydantic strict-ish validation."""
        user_id = f"test_p4_{uuid.uuid4().hex[:8]}"
        # Pydantic v2 by default ignores extra fields, so this should succeed
        # but the extra field should NOT appear in stored params
        info = mgr.create(
            user_id, "extra-field",
            strategy_key="vector:v1",
            params={"semantic_weight": 0.5, "nonexistent_param": 999},
        )
        assert "nonexistent_param" not in info.params_json


# ── Param Override Propagation ────────────────────────────────────────


class TestParamPropagation:
    def test_get_service_propagates_params(self, mgr):
        """get_service() creates a MemoryService with experiment's params."""
        user_id = f"test_p4_{uuid.uuid4().hex[:8]}"
        info = mgr.create(
            user_id, "propagation-test",
            strategy_key="vector:v1",
            params={"semantic_weight": 0.9},
        )

        # get_service should not raise — params flow through
        svc = mgr.get_service(info.experiment_id)
        assert svc is not None
        mgr.dispose_engines()

    def test_get_service_without_params_works(self, mgr):
        """get_service() works when experiment has no params."""
        user_id = f"test_p4_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "no-params-svc")

        svc = mgr.get_service(info.experiment_id)
        assert svc is not None
        mgr.dispose_engines()


# ── A/B Comparison ────────────────────────────────────────────────────


class TestABComparison:
    def test_compare_two_experiments(self, mgr, db_factory):
        """compare() returns side-by-side metrics with winners."""
        user_id = f"test_p4_{uuid.uuid4().hex[:8]}"
        exp_a = mgr.create(
            user_id, "exp-a",
            strategy_key="vector:v1",
            params={"semantic_weight": 0.6},
        )
        exp_b = mgr.create(
            user_id, "exp-b",
            strategy_key="vector:v1",
            params={"semantic_weight": 0.9},
        )

        # Simulate evaluation metrics
        mgr.update_metrics(exp_a.experiment_id, {
            "pass_rate": 0.8,
            "error_rate": 0.1,
            "sessions_tested": 10,
        })
        mgr.update_metrics(exp_b.experiment_id, {
            "pass_rate": 0.9,
            "error_rate": 0.05,
            "sessions_tested": 10,
        })

        result = mgr.compare(exp_a.experiment_id, exp_b.experiment_id)

        # Structure
        assert result["experiment_a"] == exp_a.experiment_id
        assert result["experiment_b"] == exp_b.experiment_id
        assert result["strategy_a"] == "vector:v1"
        assert result["strategy_b"] == "vector:v1"

        # Metrics present
        assert result["metrics_a"]["pass_rate"] == 0.8
        assert result["metrics_b"]["pass_rate"] == 0.9

        # Winners
        comp = result["comparison"]
        assert comp["pass_rate"]["winner"] == "b"  # 0.9 > 0.8 (higher better)
        assert comp["error_rate"]["winner"] == "b"  # 0.05 < 0.1 (lower better)
        assert comp["sessions_tested"]["winner"] is None  # not in known sets

    def test_compare_tie(self, mgr):
        """compare() reports tie when metrics are equal."""
        user_id = f"test_p4_{uuid.uuid4().hex[:8]}"
        exp_a = mgr.create(user_id, "tie-a")
        exp_b = mgr.create(user_id, "tie-b")

        mgr.update_metrics(exp_a.experiment_id, {"pass_rate": 0.85})
        mgr.update_metrics(exp_b.experiment_id, {"pass_rate": 0.85})

        result = mgr.compare(exp_a.experiment_id, exp_b.experiment_id)
        assert result["comparison"]["pass_rate"]["winner"] == "tie"

    def test_compare_different_strategies(self, mgr):
        """compare() works across different strategy types."""
        user_id = f"test_p4_{uuid.uuid4().hex[:8]}"
        exp_a = mgr.create(
            user_id, "vector-exp",
            strategy_key="vector:v1",
        )
        exp_b = mgr.create(
            user_id, "activation-exp",
            strategy_key="activation:v1",
        )

        mgr.update_metrics(exp_a.experiment_id, {
            "pass_rate": 0.7,
            "retrieval_precision_at_k": 0.6,
        })
        mgr.update_metrics(exp_b.experiment_id, {
            "pass_rate": 0.75,
            "retrieval_precision_at_k": 0.8,
            "multi_hop_hit_rate": 0.5,
        })

        result = mgr.compare(exp_a.experiment_id, exp_b.experiment_id)
        assert result["strategy_a"] == "vector:v1"
        assert result["strategy_b"] == "activation:v1"

        comp = result["comparison"]
        assert comp["pass_rate"]["winner"] == "b"
        assert comp["retrieval_precision_at_k"]["winner"] == "b"
        # multi_hop_hit_rate only in b — a has None
        assert comp["multi_hop_hit_rate"]["winner"] is None

    def test_compare_no_metrics_raises(self, mgr):
        """compare() raises when experiment has no metrics."""
        user_id = f"test_p4_{uuid.uuid4().hex[:8]}"
        exp_a = mgr.create(user_id, "no-metrics-a")
        exp_b = mgr.create(user_id, "no-metrics-b")

        with pytest.raises(ValueError, match="no metrics"):
            mgr.compare(exp_a.experiment_id, exp_b.experiment_id)

    def test_compare_nonexistent_raises(self, mgr):
        """compare() raises for nonexistent experiment."""
        user_id = f"test_p4_{uuid.uuid4().hex[:8]}"
        exp_a = mgr.create(user_id, "exists")
        mgr.update_metrics(exp_a.experiment_id, {"pass_rate": 0.5})

        with pytest.raises(ValueError, match="not found"):
            mgr.compare(exp_a.experiment_id, "nonexistent")

    def test_compare_non_numeric_metrics(self, mgr):
        """compare() handles non-numeric metrics gracefully."""
        user_id = f"test_p4_{uuid.uuid4().hex[:8]}"
        exp_a = mgr.create(user_id, "mixed-a")
        exp_b = mgr.create(user_id, "mixed-b")

        mgr.update_metrics(exp_a.experiment_id, {
            "pass_rate": 0.8,
            "note": "baseline",
        })
        mgr.update_metrics(exp_b.experiment_id, {
            "pass_rate": 0.9,
            "note": "tuned",
        })

        result = mgr.compare(exp_a.experiment_id, exp_b.experiment_id)
        assert result["comparison"]["note"]["winner"] is None
        assert result["comparison"]["pass_rate"]["winner"] == "b"


# ── Full Tuning Workflow ──────────────────────────────────────────────


class TestTuningWorkflow:
    def test_end_to_end_tuning(self, mgr, db_factory):
        """Full workflow: create with params → evaluate → compare → commit winner."""
        user_id = f"test_p4_{uuid.uuid4().hex[:8]}"

        # Create two experiments with different params
        baseline = mgr.create(
            user_id, "baseline",
            strategy_key="vector:v1",
            params={"semantic_weight": 0.4},
        )
        tuned = mgr.create(
            user_id, "tuned",
            strategy_key="vector:v1",
            params={"semantic_weight": 0.8},
        )

        # Verify both have validated params in DB
        for exp in [baseline, tuned]:
            with db_factory() as db:
                row = db.execute(
                    text(
                        "SELECT params_json FROM mem_experiments "
                        "WHERE experiment_id = :eid"
                    ),
                    {"eid": exp.experiment_id},
                ).fetchone()
                pj = row._mapping["params_json"]
                if isinstance(pj, str):
                    pj = json.loads(pj)
                assert pj is not None
                assert "semantic_weight" in pj
                assert "temporal_weight" in pj  # default filled

        # Simulate evaluation (in real usage, evaluate() would replay sessions)
        mgr.update_metrics(baseline.experiment_id, {
            "pass_rate": 0.7, "error_rate": 0.15,
        })
        mgr.update_metrics(tuned.experiment_id, {
            "pass_rate": 0.85, "error_rate": 0.05,
        })

        # Compare
        result = mgr.compare(baseline.experiment_id, tuned.experiment_id)
        assert result["comparison"]["pass_rate"]["winner"] == "b"
        assert result["comparison"]["error_rate"]["winner"] == "b"

        # Commit the winner, discard the loser
        mgr.commit(tuned.experiment_id)
        mgr.discard(baseline.experiment_id)

        # Verify final states in DB
        with db_factory() as db:
            winner = db.execute(
                text(
                    "SELECT status, committed_at, params_json "
                    "FROM mem_experiments WHERE experiment_id = :eid"
                ),
                {"eid": tuned.experiment_id},
            ).fetchone()
            loser = db.execute(
                text(
                    "SELECT status FROM mem_experiments "
                    "WHERE experiment_id = :eid"
                ),
                {"eid": baseline.experiment_id},
            ).fetchone()

            assert winner._mapping["status"] == "committed"
            assert winner._mapping["committed_at"] is not None
            pj = winner._mapping["params_json"]
            if isinstance(pj, str):
                pj = json.loads(pj)
            assert pj["semantic_weight"] == 0.8

            assert loser._mapping["status"] == "discarded"

    def test_real_evaluate_compare_workflow(self, mgr, db_factory):
        """Real workflow: create → real evaluate() → compare metrics from replay."""
        user_id = f"test_p4_{uuid.uuid4().hex[:8]}"
        session_id = f"sess_{uuid.uuid4().hex[:8]}"
        event_id = f"evt_{uuid.uuid4().hex[:8]}"
        chain_id = f"chain_{uuid.uuid4().hex[:8]}"

        # Create real session + event for replay
        with db_factory() as db:
            db.execute(
                text(
                    "INSERT INTO agent_sessions "
                    "(session_id, user_id, status, event_count, "
                    " created_at, last_active_at) "
                    "VALUES (:sid, :uid, 'active', 1, NOW(), NOW())"
                ),
                {"sid": session_id, "uid": user_id},
            )
            db.execute(
                text(
                    "INSERT INTO agent_events "
                    "(event_id, session_id, user_id, agent_id, agent_version, "
                    " event_type, content, causal_chain_id, created_at) "
                    "VALUES (:eid, :sid, :uid, 'system', '1.0.0', "
                    " 'user_query', 'test tuning query', :cid, NOW())"
                ),
                {"eid": event_id, "sid": session_id, "uid": user_id, "cid": chain_id},
            )
            db.commit()

        try:
            # Create two experiments with different params
            exp_a = mgr.create(
                user_id, "real-eval-a",
                strategy_key="vector:v1",
                params={"semantic_weight": 0.4},
            )
            exp_b = mgr.create(
                user_id, "real-eval-b",
                strategy_key="vector:v1",
                params={"semantic_weight": 0.9},
            )

            # Real evaluate() — replays the session against each experiment
            result_a = mgr.evaluate(
                exp_a.experiment_id,
                golden_session_ids=[session_id],
            )
            result_b = mgr.evaluate(
                exp_b.experiment_id,
                golden_session_ids=[session_id],
            )

            # Both should have real replay results
            assert result_a.sessions_tested == 1
            assert result_b.sessions_tested == 1
            assert result_a.replay_results[0]["replay_status"] == "completed"
            assert result_b.replay_results[0]["replay_status"] == "completed"

            # Metrics persisted in DB from real evaluate
            info_a = mgr.get(exp_a.experiment_id)
            info_b = mgr.get(exp_b.experiment_id)
            assert info_a.metrics_json is not None
            assert info_b.metrics_json is not None
            assert "pass_rate" in info_a.metrics_json
            assert "pass_rate" in info_b.metrics_json

            # Compare using real metrics from evaluate
            comparison = mgr.compare(exp_a.experiment_id, exp_b.experiment_id)
            assert comparison["strategy_a"] == "vector:v1"
            assert comparison["strategy_b"] == "vector:v1"
            assert "pass_rate" in comparison["comparison"]
            # Both replayed same session, so metrics should be equal
            assert comparison["comparison"]["pass_rate"]["winner"] == "tie"

            # Verify params survived the full round-trip
            assert comparison["metrics_a"]["sessions_tested"] == 1
            assert comparison["metrics_b"]["sessions_tested"] == 1
            assert info_a.params_json["semantic_weight"] == 0.4
            assert info_b.params_json["semantic_weight"] == 0.9
        finally:
            with db_factory() as db:
                db.execute(
                    text("DELETE FROM agent_events WHERE event_id = :eid"),
                    {"eid": event_id},
                )
                db.execute(
                    text("DELETE FROM agent_sessions WHERE session_id = :sid"),
                    {"sid": session_id},
                )
                db.commit()
