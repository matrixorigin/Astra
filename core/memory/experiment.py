"""MemoryExperiment — isolated memory experiments using Git-for-Data branching.

Lifecycle: create → mutate (via editor) → diff → evaluate → commit/discard.

Features:
- evaluate(): replay golden sessions against experiment branch (§7.3, §8.4)
- Optimistic locking on commit via base_snapshot timestamp (§7.4)
- TTL management with auto-expiry cleanup (§7.6)

See docs/design/memory/backend-management.md §7
"""

from __future__ import annotations

import contextlib
import logging
import uuid
from dataclasses import dataclass, field
from datetime import datetime as dt
from datetime import timedelta, timezone
from typing import TYPE_CHECKING, Any

from sqlalchemy import func as sa_func
from sqlalchemy import text

from core.db_consumer import DbConsumer

if TYPE_CHECKING:
    from core.db_consumer import DbFactory
    from core.memory.service import MemoryService

logger = logging.getLogger(__name__)

# Tables to branch into experiment sandbox
_MEMORY_TABLES = [
    "mem_memories",
    "memory_graph_nodes",
    "memory_graph_edges",
]

# Max active experiments per user
DEFAULT_MAX_EXPERIMENTS = 3

# Default TTL in days
DEFAULT_TTL_DAYS = 7
MAX_TTL_DAYS = 30


@dataclass
class ExperimentInfo:
    """Metadata for a memory experiment."""

    experiment_id: str
    user_id: str
    name: str
    status: str
    branch_db: str
    base_snapshot: str | None = None
    description: str = ""
    strategy_key: str | None = None
    params_json: dict[str, Any] | None = None
    metrics_json: dict[str, Any] | None = None
    created_at: dt | None = None
    committed_at: dt | None = None
    expires_at: dt | None = None


@dataclass
class ExperimentDiff:
    """Structured diff between experiment branch and production."""

    table_diffs: list[dict[str, Any]] = field(default_factory=list)
    summary: str = ""


@dataclass
class EvalResult:
    """Result of evaluating an experiment against golden sessions."""

    sessions_tested: int = 0
    sessions_passed: int = 0
    metrics: dict[str, Any] = field(default_factory=dict)
    replay_results: list[dict[str, Any]] = field(default_factory=list)


class ExperimentLimitError(Exception):
    """Raised when user exceeds max active experiments."""


class ExperimentConflictError(Exception):
    """Raised when production data changed since experiment branch point."""


def _model_to_info(row: Any) -> ExperimentInfo:
    """Convert a MemoryExperiment ORM instance to ExperimentInfo."""
    return ExperimentInfo(
        experiment_id=row.experiment_id,
        user_id=row.user_id,
        name=row.name,
        status=row.status,
        branch_db=row.branch_db,
        base_snapshot=row.base_snapshot,
        description=row.description or "",
        strategy_key=row.strategy_key,
        params_json=row.params_json,
        metrics_json=row.metrics_json,
        created_at=row.created_at,
        committed_at=row.committed_at,
        expires_at=row.expires_at,
    )


class MemoryExperimentManager(DbConsumer):
    """Manage isolated memory experiments using Git-for-Data branching.

    Each experiment creates a zero-copy branch of memory tables,
    allowing mutations without affecting production data.
    """

    def __init__(
        self,
        db_factory: DbFactory,
        source_db: str | None = None,
    ) -> None:
        super().__init__(db_factory)
        if source_db is None:
            from config.settings import get_settings
            source_db = get_settings().matrixone_database
        self._source_db = source_db
        self._branch_engines: dict[str, Any] = {}  # experiment_id → engine

    def __enter__(self) -> MemoryExperimentManager:
        return self

    def __exit__(self, *exc: object) -> None:
        self.dispose_engines()

    def create(
        self,
        user_id: str,
        name: str,
        *,
        description: str = "",
        strategy_key: str | None = None,
        params: dict[str, Any] | None = None,
        max_experiments: int = DEFAULT_MAX_EXPERIMENTS,
        ttl_days: int = DEFAULT_TTL_DAYS,
    ) -> ExperimentInfo:
        """Create an experiment: snapshot + branch memory tables.

        Args:
            user_id: Experiment owner.
            name: Human-readable experiment name.
            description: Optional description.
            strategy_key: Override strategy for this experiment.
            params: Strategy param overrides.
            max_experiments: Max active experiments per user.
            ttl_days: Days until auto-expiry (default 7, max 30).

        Returns:
            ExperimentInfo with branch_db and snapshot info.

        Raises:
            ExperimentLimitError: If user has too many active experiments.
        """
        from api.models.memory_experiment import MemoryExperiment

        ttl_days = min(ttl_days, MAX_TTL_DAYS)

        active = self._count_active(user_id)
        if active >= max_experiments:
            raise ExperimentLimitError(
                f"User {user_id} has {active} active experiments "
                f"(max {max_experiments})"
            )

        exp_id = uuid.uuid4().hex[:12]
        branch_db = f"mem_exp_{user_id[:16]}_{exp_id}"
        snapshot_name = f"base_{exp_id}"

        snapshot_ok = self._create_snapshot(snapshot_name)
        self._create_branch(branch_db, snapshot_name if snapshot_ok else None)

        now = dt.now(timezone.utc)
        record = MemoryExperiment(
            experiment_id=exp_id,
            user_id=user_id,
            name=name,
            description=description,
            status="active",
            branch_db=branch_db,
            base_snapshot=snapshot_name if snapshot_ok else None,
            strategy_key=strategy_key,
            params_json=params,
            expires_at=now + timedelta(days=ttl_days),
            created_by=user_id,
        )
        with self._db() as db:
            db.add(record)
            db.commit()

        info = self.get(exp_id)
        assert info is not None
        return info

    def get(self, experiment_id: str) -> ExperimentInfo | None:
        """Get experiment info by ID."""
        from api.models.memory_experiment import MemoryExperiment

        with self._db() as db:
            row = db.query(MemoryExperiment).filter_by(
                experiment_id=experiment_id,
            ).first()
            if row is None:
                return None
            return _model_to_info(row)

    def list_active(self, user_id: str) -> list[ExperimentInfo]:
        """List active experiments for a user."""
        from api.models.memory_experiment import MemoryExperiment

        with self._db() as db:
            rows = (
                db.query(MemoryExperiment)
                .filter_by(user_id=user_id, status="active")
                .order_by(MemoryExperiment.created_at.desc())
                .all()
            )
            return [_model_to_info(r) for r in rows]

    def get_service(
        self,
        experiment_id: str,
        *,
        llm_client: object | None = None,
        embed_fn: Any = None,
    ) -> MemoryService:
        """Get a MemoryService that reads/writes to the experiment branch.

        The returned service operates on the branch database tables,
        not production. All mutations are isolated.
        """
        info = self.get(experiment_id)
        if info is None:
            raise ValueError(f"Experiment {experiment_id} not found")
        if info.status != "active":
            raise ValueError(f"Experiment {experiment_id} is {info.status}, not active")

        branch_db_factory = self._make_branch_db_factory(info.branch_db, experiment_id)

        from core.memory.factory import create_memory_service

        return create_memory_service(
            branch_db_factory,
            strategy=info.strategy_key,
            llm_client=llm_client,
            embed_fn=embed_fn,
        )

    def diff(self, experiment_id: str) -> ExperimentDiff:
        """Diff experiment branch against production.

        Returns structured diff per memory table.
        """
        info = self.get(experiment_id)
        if info is None:
            raise ValueError(f"Experiment {experiment_id} not found")

        from core.sandbox.branch import Branch

        branch = Branch(self._db_factory, database=self._source_db)
        table_diffs: list[dict[str, Any]] = []

        for table in _MEMORY_TABLES:
            try:
                rows = branch.diff(
                    f"{info.branch_db}.{table}",
                    f"{self._source_db}.{table}",
                )
                if rows:
                    table_diffs.append({"table": table, "changes": rows})
            except Exception as e:
                logger.debug("Diff failed for %s: %s", table, e)

        return ExperimentDiff(table_diffs=table_diffs)

    # ── Evaluate ──────────────────────────────────────────────────────

    def evaluate(
        self,
        experiment_id: str,
        *,
        golden_session_ids: list[str] | None = None,
        golden_session_count: int = 50,
    ) -> EvalResult:
        """Replay golden sessions against experiment branch.

        Uses RegressionGate's golden session selection and replay
        infrastructure. Stores metrics on the experiment record.

        Args:
            experiment_id: Experiment to evaluate.
            golden_session_ids: Specific sessions to replay (optional).
            golden_session_count: Max golden sessions to auto-select.

        Returns:
            EvalResult with per-session results and aggregate metrics.
        """
        info = self.get(experiment_id)
        if info is None:
            raise ValueError(f"Experiment {experiment_id} not found")
        if info.status not in ("active", "evaluating"):
            raise ValueError(f"Experiment {experiment_id} is {info.status}")

        self._set_status(experiment_id, "evaluating")

        try:
            sessions = self._load_golden_sessions(
                golden_session_ids, golden_session_count,
            )
            if not sessions:
                result = EvalResult(metrics={"note": "no_golden_sessions"})
                self.update_metrics(experiment_id, result.metrics)
                self._set_status(experiment_id, "active")
                return result

            replay_results = self._replay_sessions(info, sessions)
            metrics = self._compute_eval_metrics(replay_results)
            result = EvalResult(
                sessions_tested=len(sessions),
                sessions_passed=sum(
                    1 for r in replay_results if r.get("successful", 0) > 0
                ),
                metrics=metrics,
                replay_results=replay_results,
            )

            self.update_metrics(experiment_id, {
                "sessions_tested": result.sessions_tested,
                "sessions_passed": result.sessions_passed,
                **metrics,
            })
            self._set_status(experiment_id, "active")
            return result

        except Exception:
            self._set_status(experiment_id, "active")
            raise

    # ── Commit with optimistic locking ────────────────────────────────

    def commit(self, experiment_id: str) -> None:
        """Merge experiment branch into production with optimistic locking.

        Compares base_snapshot timestamp against current production state.
        If production mem_memories changed since branch point, raises
        ExperimentConflictError — user must re-evaluate.

        Raises:
            ExperimentConflictError: If production changed since branch point.
        """
        from api.models.memory_experiment import MemoryExperiment

        info = self.get(experiment_id)
        if info is None:
            raise ValueError(f"Experiment {experiment_id} not found")
        if info.status != "active":
            raise ValueError(f"Experiment {experiment_id} is {info.status}, not active")

        if info.base_snapshot:
            self._check_production_unchanged(info)

        from core.sandbox.branch import Branch

        branch = Branch(self._db_factory, database=self._source_db)
        for table in _MEMORY_TABLES:
            try:
                branch.merge(
                    f"{info.branch_db}.{table}",
                    f"{self._source_db}.{table}",
                    on_conflict="accept",
                )
            except Exception as e:
                logger.debug("Merge skipped for %s: %s", table, e)

        with self._db() as db:
            db.query(MemoryExperiment).filter_by(
                experiment_id=experiment_id,
            ).update({"status": "committed", "committed_at": sa_func.now()})
            db.commit()

        self._dispose_engine(experiment_id)
        self._drop_branch_db(info.branch_db)

    def discard(self, experiment_id: str) -> None:
        """Discard experiment: drop branch DB, keep record for audit."""
        from api.models.memory_experiment import MemoryExperiment

        info = self.get(experiment_id)
        if info is None:
            raise ValueError(f"Experiment {experiment_id} not found")
        if info.status not in ("active", "evaluating"):
            raise ValueError(f"Experiment {experiment_id} is {info.status}")

        with self._db() as db:
            db.query(MemoryExperiment).filter_by(
                experiment_id=experiment_id,
            ).update({"status": "discarded"})
            db.commit()

        self._dispose_engine(experiment_id)
        self._drop_branch_db(info.branch_db)

    # ── TTL management ────────────────────────────────────────────────

    def extend_ttl(self, experiment_id: str, days: int = DEFAULT_TTL_DAYS) -> None:
        """Extend experiment TTL. Total cannot exceed MAX_TTL_DAYS from creation.

        Args:
            experiment_id: Experiment to extend.
            days: Days to add (capped so total doesn't exceed MAX_TTL_DAYS).
        """
        info = self.get(experiment_id)
        if info is None:
            raise ValueError(f"Experiment {experiment_id} not found")
        if info.status != "active":
            raise ValueError(f"Experiment {experiment_id} is {info.status}")

        # Cap: expires_at cannot exceed created_at + MAX_TTL_DAYS
        # MatrixOne doesn't support LEAST(), use CASE WHEN
        with self._db() as db:
            db.execute(
                text(
                    "UPDATE mem_experiments SET expires_at = "
                    "CASE WHEN DATE_ADD(expires_at, INTERVAL :days DAY) "
                    "       > DATE_ADD(created_at, INTERVAL :max_days DAY) "
                    "     THEN DATE_ADD(created_at, INTERVAL :max_days DAY) "
                    "     ELSE DATE_ADD(expires_at, INTERVAL :days DAY) "
                    "END "
                    "WHERE experiment_id = :eid"
                ),
                {"eid": experiment_id, "days": days, "max_days": MAX_TTL_DAYS},
            )
            db.commit()

    def cleanup_expired(self, *, drop_snapshots: bool = False) -> int:
        """Expire and clean up experiments past their TTL.

        Sets status='expired', drops branch DBs.
        Optionally drops base snapshots (default: retained for audit per §7.6).
        Intended to be called by a daily governance job.

        Args:
            drop_snapshots: If True, also drop base_snapshot (saves storage).

        Returns:
            Number of experiments expired.
        """
        from api.models.memory_experiment import MemoryExperiment

        with self._db() as db:
            rows = (
                db.query(MemoryExperiment)
                .filter(
                    MemoryExperiment.status == "active",
                    MemoryExperiment.expires_at.isnot(None),
                    MemoryExperiment.expires_at < sa_func.now(),
                )
                .all()
            )
            expired = [
                {
                    "experiment_id": r.experiment_id,
                    "branch_db": r.branch_db,
                    "base_snapshot": r.base_snapshot,
                }
                for r in rows
            ]

        count = 0
        for item in expired:
            exp_id = item["experiment_id"]
            try:
                self._set_status(exp_id, "expired")
                self._drop_branch_db(item["branch_db"])
                if drop_snapshots and item.get("base_snapshot"):
                    self._drop_snapshot(item["base_snapshot"])
                count += 1
            except Exception:
                logger.warning("Failed to expire experiment %s", exp_id, exc_info=True)

        return count

    # ── Metrics ───────────────────────────────────────────────────────

    def update_metrics(
        self, experiment_id: str, metrics: dict[str, Any],
    ) -> None:
        """Store evaluation metrics on the experiment record."""
        from api.models.memory_experiment import MemoryExperiment

        with self._db() as db:
            db.query(MemoryExperiment).filter_by(
                experiment_id=experiment_id,
            ).update({"metrics_json": metrics})
            db.commit()

    # ── Internal helpers ──────────────────────────────────────────────

    def _set_status(self, experiment_id: str, status: str) -> None:
        from api.models.memory_experiment import MemoryExperiment

        with self._db() as db:
            db.query(MemoryExperiment).filter_by(
                experiment_id=experiment_id,
            ).update({"status": status})
            db.commit()

    def _count_active(self, user_id: str) -> int:
        from api.models.memory_experiment import MemoryExperiment

        with self._db() as db:
            return (
                db.query(sa_func.count())
                .select_from(MemoryExperiment)
                .filter_by(user_id=user_id, status="active")
                .scalar()
            ) or 0

    def _create_snapshot(self, name: str) -> bool:
        """Create account-level snapshot. Returns True on success."""
        try:
            from core.git_for_data import GitForData

            git = GitForData(self._db_factory)
            git.create_snapshot(name)
            return True
        except Exception:
            logger.warning("Failed to create experiment snapshot %s", name, exc_info=True)
            return False

    def _create_branch(self, branch_db: str, snapshot: str | None) -> None:
        """Create branch database with memory tables."""
        from core.sandbox.branch import Branch

        branch = Branch(self._db_factory, database=self._source_db)

        with self._db() as db:
            db.commit()
            db.execute(text(f"DROP DATABASE IF EXISTS `{branch_db}`"))
            db.commit()
            db.execute(text(f"CREATE DATABASE `{branch_db}`"))
            db.commit()

        for table in _MEMORY_TABLES:
            try:
                branch.create(
                    f"{branch_db}.{table}",
                    f"{self._source_db}.{table}",
                    snapshot=snapshot,
                )
            except Exception as e:
                logger.debug("Branch table %s failed: %s", table, e)

    def _drop_branch_db(self, branch_db: str) -> None:
        """Drop branch database. Best-effort."""
        try:
            from core.sandbox.branch import Branch

            branch = Branch(self._db_factory, database=self._source_db)
            for table in _MEMORY_TABLES:
                with contextlib.suppress(Exception):
                    branch.delete(f"{branch_db}.{table}")

            with self._db() as db:
                db.commit()
                db.execute(text(f"DROP DATABASE IF EXISTS `{branch_db}`"))
                db.commit()
        except Exception:
            logger.warning("Failed to drop branch DB %s", branch_db, exc_info=True)

    def _drop_snapshot(self, name: str) -> None:
        """Drop a snapshot. Best-effort."""
        try:
            from core.git_for_data import GitForData

            git = GitForData(self._db_factory)
            git.drop_snapshot(name)
        except Exception:
            logger.debug("Failed to drop snapshot %s", name, exc_info=True)

    def _make_branch_db_factory(self, branch_db: str, experiment_id: str) -> DbFactory:
        """Create a db_factory that connects to the branch database.

        The engine is tracked and can be disposed via dispose_engines().
        """
        from sqlalchemy import create_engine
        from sqlalchemy.orm import sessionmaker

        from config.settings import get_settings

        settings = get_settings()
        url = (
            f"mysql+pymysql://{settings.matrixone_user}:{settings.matrixone_password}"
            f"@{settings.matrixone_host}:{settings.matrixone_port}/{branch_db}"
            "?charset=utf8mb4"
        )
        eng = create_engine(url, pool_pre_ping=True, pool_size=2)
        self._branch_engines[experiment_id] = eng
        factory = sessionmaker(bind=eng)
        return factory  # type: ignore[return-value]

    def dispose_engines(self) -> None:
        """Dispose all branch database engines to release connections."""
        for eng in self._branch_engines.values():
            with contextlib.suppress(Exception):
                eng.dispose()
        self._branch_engines.clear()

    def _dispose_engine(self, experiment_id: str) -> None:
        """Dispose a single branch engine if tracked."""
        eng = self._branch_engines.pop(experiment_id, None)
        if eng is not None:
            with contextlib.suppress(Exception):
                eng.dispose()

    def _check_production_unchanged(self, info: ExperimentInfo) -> None:
        """Optimistic lock: verify production hasn't changed since branch point.

        Compares snapshot timestamp against max(updated_at) in production
        mem_memories for this user. If production has newer writes,
        the experiment's assumptions may be stale.
        """
        from api.models.memory import MemoryRecord
        from core.git_for_data import GitForData

        git = GitForData(self._db_factory)
        snap_info = git.get_snapshot_info(info.base_snapshot)  # type: ignore[arg-type]
        if snap_info is None:
            logger.warning(
                "Base snapshot %s not found, skipping optimistic lock",
                info.base_snapshot,
            )
            return

        snap_ts = snap_info.get("timestamp")
        if snap_ts is None:
            return

        with self._db() as db:
            cnt = (
                db.query(sa_func.count())
                .select_from(MemoryRecord)
                .filter(
                    MemoryRecord.user_id == info.user_id,
                    MemoryRecord.updated_at > snap_ts,
                )
                .scalar()
            ) or 0
            if cnt > 0:
                raise ExperimentConflictError(
                    f"Production has {cnt} memory changes since branch point "
                    f"({snap_ts}). Re-evaluate or discard the experiment."
                )

    def _load_golden_sessions(
        self,
        session_ids: list[str] | None,
        limit: int,
    ) -> list[dict[str, Any]]:
        """Load golden sessions for evaluation.

        If session_ids provided, use those. Otherwise auto-select
        high-quality sessions via the same criteria as RegressionGate.
        """
        from api.models.agent import Event

        if session_ids:
            with self._db() as db:
                rows = (
                    db.query(Event.session_id, Event.user_id)
                    .filter(Event.session_id.in_(session_ids))
                    .distinct()
                    .all()
                )
                return [
                    {"session_id": r.session_id, "user_id": r.user_id,
                     "avg_score": 0.0}
                    for r in rows
                ]

        cutoff = dt.now(timezone.utc) - timedelta(days=30)
        with self._db() as db:
            rows = (
                db.query(
                    Event.session_id,
                    Event.user_id,
                    sa_func.avg(Event.quality_score).label("avg_score"),
                )
                .filter(
                    Event.quality_score >= 4.0,
                    Event.training_eligible == 1,
                    Event.created_at > cutoff,
                )
                .group_by(Event.session_id, Event.user_id)
                .having(sa_func.count() >= 3)
                .order_by(sa_func.avg(Event.quality_score).desc())
                .limit(limit)
                .all()
            )
            return [
                {"session_id": r.session_id, "user_id": r.user_id,
                 "avg_score": float(r.avg_score)}
                for r in rows
            ]

    def _replay_sessions(
        self,
        info: ExperimentInfo,
        sessions: list[dict[str, Any]],
    ) -> list[dict[str, Any]]:
        """Replay sessions against experiment branch.

        Uses ReplayService to replay each session in the experiment's
        branch database context.

        Raises:
            ImportError: If ReplayService is not available.
        """
        from api.services.replay_service import ReplayService

        results: list[dict[str, Any]] = []
        for session in sessions:
            try:
                with self._db() as db:
                    replay_svc = ReplayService(lambda db=db: db)
                    result = replay_svc.replay_session(
                        session_id=session["session_id"],
                        user_id=session["user_id"],
                        sandbox_name=info.branch_db,
                        mock_mode=True,
                    )
                    results.append({
                        "session_id": session["session_id"],
                        "original_score": session.get("avg_score", 0.0),
                        "replay_status": result.get("status", "unknown"),
                        "successful": result.get("result", {}).get("successful", 0),
                        "failed": result.get("result", {}).get("failed", 0),
                    })
            except Exception as e:
                results.append({
                    "session_id": session["session_id"],
                    "original_score": session.get("avg_score", 0.0),
                    "replay_status": "error",
                    "error": str(e),
                    "successful": 0,
                    "failed": 1,
                })

        return results

    @staticmethod
    def _compute_eval_metrics(
        replay_results: list[dict[str, Any]],
    ) -> dict[str, Any]:
        """Compute aggregate metrics from replay results."""
        total = len(replay_results)
        if total == 0:
            return {"sessions_tested": 0}

        passed = sum(1 for r in replay_results if r.get("successful", 0) > 0)
        failed = sum(1 for r in replay_results if r.get("replay_status") == "error")

        return {
            "sessions_tested": total,
            "pass_rate": passed / total if total else 0.0,
            "error_rate": failed / total if total else 0.0,
        }
