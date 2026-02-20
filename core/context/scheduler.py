"""Memory governance scheduler — abstract interface + pluggable backends.

Architecture:
    GovernanceTaskRunner  (abstract)  — what to run
    SchedulerBackend      (abstract)  — how to schedule
    MemoryGovernanceScheduler         — wires them together

Backends:
    AsyncIOBackend   — single-process, zero dependencies (dev/small deploy)
    (pluggable)      — Celery, APScheduler, Temporal, K8s CronJob, etc.

Distributed safety:
    TaskRunner acquires a DB lock (distributed_locks table) before executing.
    Multiple instances can run the same schedule — only one wins per cycle.
    Lock expires after heartbeat_timeout to handle crashed instances.
"""

from __future__ import annotations

import asyncio
import os
import socket
from abc import ABC, abstractmethod
from contextlib import contextmanager
from datetime import datetime, timedelta
from typing import Any, Callable

from sqlalchemy.orm import Session

from core.logging_config import get_logger

logger = get_logger(__name__)

# ── Task definitions ────────────────────────────────────────────────

GOVERNANCE_TASKS: dict[str, dict[str, Any]] = {
    "hourly":  {"interval": 3600,   "lock_name": "governance_hourly"},
    "daily":   {"interval": 86400,  "lock_name": "governance_daily"},
    "weekly":  {"interval": 604800, "lock_name": "governance_weekly"},
}

# Lock expiry: if instance crashes, lock auto-expires after this
LOCK_HEARTBEAT_TIMEOUT = 300  # 5 minutes


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


class GovernanceTaskRunner:
    """Executes governance tasks with distributed locking.

    Each task acquires a DB-level lock (distributed_locks table) so that
    across N replicas, only one instance runs a given task per cycle.
    """

    def __init__(self, db_context_factory: Callable):
        """Args:
            db_context_factory: callable returning a context-manager that yields a Session.
                                e.g. ``api.database.get_db_context``
        """
        self._db_ctx = db_context_factory
        self._instance_id = self._get_instance_id()

    @staticmethod
    def _get_instance_id() -> str:
        """Generate unique instance ID: hostname:pid"""
        return f"{socket.gethostname()}:{os.getpid()}"

    def run(self, task_name: str) -> dict[str, int] | None:
        """Run a governance task if lock is acquired.

        Returns:
            Task results dict, or None if another instance holds the lock.
        """
        lock_name = GOVERNANCE_TASKS[task_name]["lock_name"]
        method = f"run_{task_name}_tasks"

        with self._db_ctx() as db:
            if not self._try_acquire_lock(db, lock_name):
                logger.debug(f"Governance [{task_name}]: skipped (lock held by another instance)")
                return None

            try:
                from core.context.lifecycle import MemoryGovernanceEngine
                result = getattr(MemoryGovernanceEngine(db), method)()
                logger.info(f"Governance [{task_name}]: {result}")
                return result
            finally:
                self._release_lock(db, lock_name)

    def _try_acquire_lock(self, db: Session, lock_name: str) -> bool:
        """Try to acquire a distributed lock.
        
        Returns True if lock acquired, False if held by another instance.
        """
        from api.models import DistributedLock

        now = datetime.now()
        expires_at = now + timedelta(seconds=LOCK_HEARTBEAT_TIMEOUT)

        try:
            # Try to insert new lock (will fail if lock_name already exists)
            lock = DistributedLock(
                lock_name=lock_name,
                instance_id=self._instance_id,
                acquired_at=now,
                expires_at=expires_at,
                task_name=lock_name.split("_", 1)[1],  # e.g. "hourly" from "governance_hourly"
            )
            db.add(lock)
            db.commit()
            return True
        except Exception:
            # Lock already exists; check if expired
            try:
                existing = db.query(DistributedLock).filter(
                    DistributedLock.lock_name == lock_name
                ).first()
                
                if existing and existing.expires_at < now:
                    # Lock expired; take it over
                    existing.instance_id = self._instance_id
                    existing.acquired_at = now
                    existing.expires_at = expires_at
                    db.commit()
                    return True
                
                return False
            except Exception as e:
                logger.error(f"Lock check failed: {e}")
                return False

    @staticmethod
    def _release_lock(db: Session, lock_name: str) -> None:
        """Release a distributed lock."""
        from api.models import DistributedLock

        try:
            db.query(DistributedLock).filter(
                DistributedLock.lock_name == lock_name
            ).delete()
            db.commit()
        except Exception as e:
            logger.error(f"Lock release failed: {e}")

# ── Default backend: asyncio (single-process) ──────────────────────

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

    Usage (default — asyncio):
        scheduler = MemoryGovernanceScheduler()
        await scheduler.start()
        ...
        await scheduler.stop()

    Usage (custom backend — e.g. Celery):
        runner = GovernanceTaskRunner(get_db_context)
        backend = CeleryBackend(runner)          # you implement this
        scheduler = MemoryGovernanceScheduler(backend=backend)
        await scheduler.start()
    """

    def __init__(self, backend: SchedulerBackend | None = None):
        if backend is None:
            from api.database import get_db_context
            runner = GovernanceTaskRunner(get_db_context)
            backend = AsyncIOBackend(runner)
        self._backend = backend

    async def start(self) -> None:
        tasks = {name: cfg["interval"] for name, cfg in GOVERNANCE_TASKS.items()}
        await self._backend.start(tasks)
        logger.info("Memory governance scheduler started")

    async def stop(self) -> None:
        await self._backend.stop()
        logger.info("Memory governance scheduler stopped")
