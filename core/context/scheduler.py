"""Governance task scheduler — minimal implementation for sandbox cleanup and SLO monitoring.

Runs periodic background tasks with distributed locking for multi-instance safety.
Memory governance is handled by Memoria.
"""

from __future__ import annotations

import asyncio
import os
import socket
from abc import ABC, abstractmethod
from datetime import datetime, timedelta
from typing import Any, Callable

from sqlalchemy import text
from sqlalchemy.orm import Session

from core.logging_config import get_logger

logger = get_logger(__name__)


# Task definitions
GOVERNANCE_TASKS = {
    "hourly": {"interval": 3600, "lock_name": "governance_hourly"},
    "daily": {"interval": 86400, "lock_name": "governance_daily"},
    "weekly": {"interval": 604800, "lock_name": "governance_weekly"},
    "eval_daily": {"interval": 86400, "lock_name": "governance_eval_daily"},
}

LOCK_TTL = 300  # 5 minutes
GOVERNANCE_ENABLED = os.getenv("GOVERNANCE_ENABLED", "1") == "1"


class SchedulerBackend(ABC):
    """Abstract interface for task scheduling backends."""

    @abstractmethod
    async def start(self, tasks: dict[str, int]) -> None:
        """Start periodic tasks."""
        pass

    @abstractmethod
    async def stop(self) -> None:
        """Stop all tasks."""
        pass


class GovernanceTaskRunner:
    """Executes governance tasks with distributed locking.
    
    Uses infra_distributed_locks table for coordination across multiple instances.
    """

    def __init__(self, db_context_factory: Callable):
        self._db_ctx = db_context_factory
        self._instance_id = f"{socket.gethostname()}:{os.getpid()}"

    def run(self, task_name: str) -> dict[str, int] | None:
        """Run a governance task if lock is acquired."""
        if not GOVERNANCE_ENABLED:
            return None

        lock_name = GOVERNANCE_TASKS[task_name]["lock_name"]

        with self._db_ctx() as db:
            if not self._try_acquire(db, lock_name):
                logger.debug(f"Governance [{task_name}]: skipped (lock held)")
                return None

            try:
                result = self._execute_task(db, task_name)
                db.commit()
                logger.info(f"Governance [{task_name}]: completed {result}")
                return result
            except Exception as e:
                logger.error(f"Governance [{task_name}]: failed: {e}", exc_info=True)
                db.rollback()
                raise
            finally:
                self._release(db, lock_name)

    def _try_acquire(self, db: Session, lock_name: str) -> bool:
        """Try to acquire distributed lock."""
        now = datetime.now()
        expires_at = now + timedelta(seconds=LOCK_TTL)
        
        try:
            # Try INSERT (new lock)
            db.execute(
                text("""
                    INSERT INTO infra_distributed_locks (lock_name, instance_id, acquired_at, expires_at, task_name)
                    VALUES (:name, :holder, :now, :expires, :task)
                """),
                {"name": lock_name, "holder": self._instance_id, "now": now, "expires": expires_at, "task": lock_name}
            )
            db.commit()
            return True
        except Exception:
            # Lock exists, try to take if expired
            result = db.execute(
                text("""
                    UPDATE infra_distributed_locks
                    SET instance_id = :holder, acquired_at = :now, expires_at = :expires
                    WHERE lock_name = :name AND expires_at < :now
                """),
                {"name": lock_name, "holder": self._instance_id, "now": now, "expires": expires_at}
            )
            db.commit()
            return result.rowcount > 0

    def _release(self, db: Session, lock_name: str) -> None:
        """Release distributed lock."""
        db.execute(
            text("DELETE FROM infra_distributed_locks WHERE lock_name = :name AND instance_id = :holder"),
            {"name": lock_name, "holder": self._instance_id}
        )
        db.commit()

    def _execute_task(self, db: Session, task_name: str) -> dict[str, int]:
        """Execute the actual governance task."""
        if task_name == "eval_daily":
            return self._run_eval_daily(db)
        
        from core.context.lifecycle import MemoryGovernanceEngine
        
        # Get db_factory from context manager
        engine = MemoryGovernanceEngine(db_factory=lambda: db)
        
        if task_name == "hourly":
            return engine.run_hourly_tasks()
        elif task_name == "daily":
            return engine.run_daily_tasks()
        elif task_name == "weekly":
            return engine.run_weekly_tasks()
        else:
            return {}

    def _run_eval_daily(self, db: Session) -> dict[str, int]:
        """Run evaluation closed-loop tasks."""
        # Placeholder for evaluation tasks
        # TODO: Implement drift detection, calibration, learning
        return {"drift_signals": 0, "skills_learned": 0}


class AsyncIOBackend(SchedulerBackend):
    """Simple asyncio-based scheduler for single-process deployments."""

    def __init__(self, runner: GovernanceTaskRunner):
        self._runner = runner
        self._tasks: list[asyncio.Task] = []
        self._running = False

    async def start(self, tasks: dict[str, int]) -> None:
        """Start periodic tasks."""
        self._running = True
        for name, interval in tasks.items():
            task = asyncio.create_task(self._run_periodic(name, interval))
            self._tasks.append(task)
        logger.info(f"Scheduler started: {list(tasks.keys())}")

    async def stop(self) -> None:
        """Stop all tasks."""
        self._running = False
        for task in self._tasks:
            task.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        logger.info("Scheduler stopped")

    async def _run_periodic(self, task_name: str, interval: int) -> None:
        """Run a task periodically."""
        while self._running:
            try:
                await asyncio.sleep(interval)
                if self._running:
                    # Run in thread pool to avoid blocking
                    loop = asyncio.get_event_loop()
                    await loop.run_in_executor(None, self._runner.run, task_name)
            except asyncio.CancelledError:
                break
            except Exception as e:
                logger.error(f"Periodic task [{task_name}] error: {e}")


class MemoryGovernanceScheduler:
    """Facade for governance scheduling."""

    def __init__(self, db_context_factory: Callable | None = None, backend: SchedulerBackend | None = None):
        self._enabled = GOVERNANCE_ENABLED
        self._runner = GovernanceTaskRunner(db_context_factory) if db_context_factory else None
        self._backend = backend or (AsyncIOBackend(self._runner) if self._enabled and self._runner else None)

    async def start(self) -> None:
        """Start the scheduler."""
        if not self._enabled:
            logger.info("Governance scheduler disabled (GOVERNANCE_ENABLED=0)")
            return

        if not self._backend:
            logger.warning("Governance scheduler: no backend configured")
            return

        tasks = {name: cfg["interval"] for name, cfg in GOVERNANCE_TASKS.items()}
        await self._backend.start(tasks)

    async def stop(self) -> None:
        """Stop the scheduler."""
        if self._backend:
            await self._backend.stop()
