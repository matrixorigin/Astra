"""Memory governance scheduler — abstract interface + pluggable backends.

Architecture:
    GovernanceTaskRunner  — what to run (with distributed locking)
    SchedulerBackend      (abstract)  — how to schedule
    MemoryGovernanceScheduler         — wires them together

Backends:
    AsyncIOBackend   — single-process, zero dependencies (dev/small deploy)
    (pluggable)      — Celery, APScheduler, Temporal, K8s CronJob, etc.

Distributed safety:
    - DB table lock (infra_distributed_locks) with heartbeat renewal
    - Atomic CAS for expired lock takeover
    - Safe across multi-worker / multi-instance deployments
    - Controlled via GOVERNANCE_ENABLED env var
"""

from __future__ import annotations

import asyncio
import os
import socket
import threading
from abc import ABC, abstractmethod
from datetime import datetime, timedelta
from typing import Any, Callable

from sqlalchemy import text
from sqlalchemy.exc import IntegrityError, OperationalError
from sqlalchemy.orm import Session

from core.logging_config import get_logger

logger = get_logger(__name__)

# ── Task definitions ────────────────────────────────────────────────

GOVERNANCE_TASKS: dict[str, dict[str, Any]] = {
    "hourly":  {"interval": 3600,   "lock_name": "governance_hourly"},
    "daily":   {"interval": 86400,  "lock_name": "governance_daily"},
    "weekly":  {"interval": 604800, "lock_name": "governance_weekly"},
    # Evaluation closed-loop tasks — auto-trigger drift/calibration/learning
    "eval_daily": {"interval": 86400, "lock_name": "governance_eval_daily"},
}

LOCK_TTL = 300          # 5 min — lock expires if no heartbeat
HEARTBEAT_INTERVAL = 60  # renew every 60s during task execution


# ── Abstract interfaces ────────────────────────────────────────────

class SchedulerBackend(ABC):
    """How tasks are scheduled. Swap for any distributed scheduler."""

    @abstractmethod
    async def start(self, tasks: dict[str, int]) -> None:
        """Register and start periodic tasks.

        Args:
            tasks: mapping of task_name → interval_seconds
        """

    @abstractmethod
    async def stop(self) -> None:
        """Gracefully stop all scheduled tasks."""


# ── Task runner with distributed locking ────────────────────────────

class GovernanceTaskRunner:
    """Executes governance tasks with distributed locking + heartbeat.

    Safety guarantees:
    - Atomic lock acquisition (INSERT) and takeover (UPDATE ... WHERE)
    - Heartbeat thread renews expires_at during long tasks
    - Rollback on exception before lock release
    - Multiple workers/instances safe — only one wins per cycle
    """

    def __init__(self, db_context_factory: Callable):
        self._db_ctx = db_context_factory
        self._instance_id = f"{socket.gethostname()}:{os.getpid()}"

    def run(self, task_name: str) -> dict[str, int] | None:
        """Run a governance task if lock is acquired."""
        lock_name = GOVERNANCE_TASKS[task_name]["lock_name"]

        with self._db_ctx() as db:
            if not self._try_acquire(db, lock_name):
                logger.debug(f"Governance [{task_name}]: skipped (lock held)")
                return None

            # Start heartbeat thread to renew lock during execution
            stop_heartbeat = threading.Event()
            hb = threading.Thread(
                target=self._heartbeat_loop,
                args=(lock_name, self._instance_id, stop_heartbeat),
                daemon=True,
            )
            hb.start()

            try:
                # SessionLocal (raw factory → Session) rather than self._db_ctx
                # (context-manager factory) because phases need db_factory() → Session
                # with caller-managed close(), not `with db_ctx() as db:`.
                from api.database import SessionLocal
                result = self._dispatch(task_name, db, SessionLocal)
                self._persist_run(db, task_name, result)
                logger.info(f"Governance [{task_name}]: {result}")
                return result
            except Exception as e:
                logger.error(f"Governance [{task_name}] task error: {e}")
                db.rollback()
                return None
            finally:
                stop_heartbeat.set()
                hb.join(timeout=5)
                self._release(db, lock_name)

    # ── Task dispatch ──────────────────────────────────────────

    @staticmethod
    def _dispatch(task_name: str, db: Session, db_factory: Callable) -> dict[str, int]:
        """Route task_name to the appropriate executor.

        Runs both:
        - MemoryGovernanceEngine (sk_knowledge_entries governance)
        - GovernanceScheduler (memories table governance)

        Args:
            db: Lock-holding session (for MemoryGovernanceEngine tasks).
            db_factory: Factory for independent sessions.
        """
        if task_name == "eval_daily":
            return GovernanceTaskRunner._run_eval_daily(db_factory)

        results: dict[str, int] = {}

        # 1. Knowledge entries governance (lifecycle.py)
        try:
            from core.context.lifecycle import MemoryGovernanceEngine
            engine = MemoryGovernanceEngine(db_factory)
            ke_results = getattr(engine, f"run_{task_name}_tasks")()
            results.update(ke_results)
        except Exception as e:
            logger.error("Knowledge governance [%s] failed: %s", task_name, e)

        # 2. Memories table governance (via MemoryService facade)
        try:
            from core.memory.tabular.service import MemoryService
            svc = MemoryService(db_factory)
            if task_name == "hourly":
                r = svc.run_hourly()
                results["mem_cleaned_tool_results"] = r.cleaned_tool_results
                results["mem_archived_working"] = r.archived_working
            elif task_name == "daily":
                r = svc.run_daily_all()
                results["mem_cleaned_stale"] = r.cleaned_stale
                results["mem_quarantined"] = r.quarantined
            elif task_name == "weekly":
                r = svc.run_weekly()
                results["mem_cleaned_branches"] = r.cleaned_branches
                results["mem_cleaned_snapshots"] = r.cleaned_snapshots
            results.update({f"mem_{k}": v for k, v in r.__dict__.items() if k == "errors" and v})
        except Exception as e:
            logger.error("Memory governance [%s] failed: %s", task_name, e)

        return results
    @staticmethod
    def _run_eval_daily(db_factory: Callable) -> dict[str, int]:
        """Run daily evaluation closed-loop: drift → calibration → learning.

        Each phase gets its own short-lived session from *db_factory* so that
        a failure in one phase cannot rollback or corrupt another's work.
        The caller's lock-holding session is never touched.
        """
        results: dict[str, int] = {}

        # Phase 1: Drift detection + auto-correction
        try:
            from core.evaluation.drift_pipeline import run_drift_pipeline
            drift = run_drift_pipeline(db_factory=db_factory)
            results["drift_signals"] = drift.signals_detected
            results["drift_corrections"] = drift.corrections_applied
        except Exception as e:
            logger.error("eval_daily drift failed: %s", e)
            results["drift_signals"] = 0

        # Phase 2: Confidence calibration
        try:
            from core.evaluation.confidence_calibrator import ConfidenceCalibrator
            cal = ConfidenceCalibrator(db_factory)
            cal_result = cal.measure(days=7)
            results["calibration_error"] = round(cal_result.calibration_error * 100)
        except Exception as e:
            logger.error("eval_daily calibration failed: %s", e)

        # Phase 3: Input face learning (prompt, context budget, knowledge)
        try:
            from core.learning.input_face_learner import InputFaceLearner
            from core.llm.client import LLMClient
            llm = LLMClient(db_factory)
            learner = InputFaceLearner(db_factory, llm)
            face_results = learner.diagnose_and_fix(days=7)
            results["faces_fixed"] = sum(1 for r in face_results if r.applied)
        except Exception as e:
            logger.error("eval_daily learning failed: %s", e)

        # Phase 4: Skill selection learning
        try:
            # Self-improving selector removed in skill system cleanup
            results["skills_learned"] = 0
        except Exception as e:
            logger.error("eval_daily skill learning failed: %s", e)

        return results

    # ── Lock operations ─────────────────────────────────────────

    def _try_acquire(self, db: Session, lock_name: str) -> bool:
        """Try INSERT; on conflict, atomic CAS takeover if expired."""
        now = datetime.now()
        expires_at = now + timedelta(seconds=LOCK_TTL)

        # Fast path: try insert
        try:
            from api.models import DistributedLock
            db.add(DistributedLock(
                lock_name=lock_name,
                instance_id=self._instance_id,
                acquired_at=now,
                expires_at=expires_at,
                task_name=lock_name.split("_", 1)[1],
            ))
            db.commit()
            return True
        except (IntegrityError, OperationalError):
            db.rollback()

        # Slow path: atomic CAS — take over only if expired
        result = db.execute(
            text(
                "UPDATE infra_distributed_locks "
                "SET instance_id = :iid, acquired_at = :now, expires_at = :exp "
                "WHERE lock_name = :name AND expires_at < :now"
            ),
            {"iid": self._instance_id, "now": now, "exp": expires_at, "name": lock_name},
        )
        db.commit()
        return result.rowcount > 0

    @staticmethod
    def _release(db: Session, lock_name: str) -> None:
        try:
            db.execute(
                text("DELETE FROM infra_distributed_locks WHERE lock_name = :name"),
                {"name": lock_name},
            )
            db.commit()
        except Exception as e:
            logger.error(f"Lock release failed: {e}")

    # ── Audit persistence ──────────────────────────────────────

    @staticmethod
    def _persist_run(db: Session, task_name: str, result: dict[str, int]) -> None:
        """Write governance run result to governance_runs for trend tracking."""
        import json
        try:
            db.execute(
                text(
                    "INSERT INTO governance_runs (task_name, result, created_at) "
                    "VALUES (:task, :result, :ts)"
                ),
                {
                    "task": task_name,
                    "result": json.dumps(result),
                    "ts": datetime.now(),
                },
            )
            db.commit()
        except Exception as e:
            # Table may not exist yet — log and continue, don't break governance
            logger.debug("governance_runs write skipped: %s", e)
            db.rollback()

    # ── Heartbeat ───────────────────────────────────────────────

    def _heartbeat_loop(self, lock_name: str, instance_id: str, stop: threading.Event):
        """Renew lock expires_at periodically during task execution."""
        while not stop.wait(HEARTBEAT_INTERVAL):
            try:
                with self._db_ctx() as db:
                    new_exp = datetime.now() + timedelta(seconds=LOCK_TTL)
                    db.execute(
                        text(
                            "UPDATE infra_distributed_locks "
                            "SET expires_at = :exp "
                            "WHERE lock_name = :name AND instance_id = :iid"
                        ),
                        {"exp": new_exp, "name": lock_name, "iid": instance_id},
                    )
                    db.commit()
            except Exception as e:
                logger.warning(f"Heartbeat renewal failed: {e}")


# ── Default backend: asyncio ───────────────────────────────────────

class AsyncIOBackend(SchedulerBackend):
    """Zero-dependency asyncio backend. Good for dev and small deploys."""

    def __init__(self, runner: GovernanceTaskRunner):
        self._runner = runner
        self._async_tasks: list[asyncio.Task] = []

    async def start(self, tasks: dict[str, int]) -> None:
        self._async_tasks = [
            asyncio.create_task(self._loop(name, interval))
            for name, interval in tasks.items()
        ]

    async def stop(self) -> None:
        for t in self._async_tasks:
            t.cancel()
        await asyncio.gather(*self._async_tasks, return_exceptions=True)
        self._async_tasks.clear()

    async def _loop(self, name: str, interval: int) -> None:
        while True:
            await asyncio.sleep(interval)
            try:
                self._runner.run(name)
            except asyncio.CancelledError:
                raise
            except Exception as e:
                logger.error(f"Governance [{name}] failed: {e}")


# ── Public façade ───────────────────────────────────────────────────

class MemoryGovernanceScheduler:
    """Façade: wires task runner + backend.

    Controlled by GOVERNANCE_ENABLED env var (default: true).
    Safe to start in every worker — lock ensures single execution.

    Usage (default — asyncio):
        scheduler = MemoryGovernanceScheduler()
        await scheduler.start()

    Usage (custom backend):
        scheduler = MemoryGovernanceScheduler(backend=my_celery_backend)
        await scheduler.start()
    """

    # TODO(future-arch): Replace direct DB access in GovernanceTaskRunner
    # with internal API calls when moving to distributed deployment.

    def __init__(self, backend: SchedulerBackend | None = None):
        self._enabled = os.environ.get("GOVERNANCE_ENABLED", "true").lower() == "true"
        if backend is None and self._enabled:
            from api.database import get_db_context
            runner = GovernanceTaskRunner(get_db_context)
            backend = AsyncIOBackend(runner)
        self._backend = backend

    async def start(self) -> None:
        if not self._enabled:
            logger.info("Memory governance scheduler disabled (GOVERNANCE_ENABLED=false)")
            return
        tasks = {name: cfg["interval"] for name, cfg in GOVERNANCE_TASKS.items()}
        await self._backend.start(tasks)
        logger.info("Memory governance scheduler started")

    async def stop(self) -> None:
        if not self._enabled or not self._backend:
            return
        await self._backend.stop()
        logger.info("Memory governance scheduler stopped")
