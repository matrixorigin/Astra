"""Regression gate auto-trigger — fire gate on versioned input changes.

Ref: trust-and-safety.md §4
Triggers: skill_version_changed, prompt_template_changed

Distributed-safe: uses distributed_locks table (same mechanism as
MemoryGovernanceScheduler) to ensure only ONE instance runs the gate
per change_id across N replicas.

Runs gate asynchronously (background thread) so callers are not blocked.
"""

from __future__ import annotations

import threading
from datetime import datetime, timedelta
from typing import Any

from sqlalchemy.exc import IntegrityError, OperationalError

from core.evaluation.regression_gate import ChangeType, RegressionGate
from core.logging_config import get_logger

logger = get_logger(__name__)

_LOCK_TTL_SECONDS = 300  # gate can take up to 5 min


class GateTrigger:
    """Fires regression gate asynchronously on versioned input changes.

    Distributed-safe: only one instance across N replicas will execute
    the gate for a given change_id (first writer wins via DB lock).
    """

    def __init__(self, db_factory, account: str = "sys"):
        """
        Args:
            db_factory: Callable[[], Session] — creates a fresh DB session.
            account: MatrixOne account for sandbox creation.
        """
        self._db_factory = db_factory
        self._account = account

    def on_skill_change(self, skill_name: str, version: str, definition: dict[str, Any]):
        """Trigger gate after a skill version is registered."""
        self._fire_async(
            change_type="skill",
            change_id=f"{skill_name}@{version}",
            change_content={"name": skill_name, "version": version, "definition": definition},
        )

    def on_prompt_change(self, template_id: str, version: str, content: str):
        """Trigger gate after a prompt template version is registered."""
        self._fire_async(
            change_type="prompt",
            change_id=f"{template_id}@{version}",
            change_content={"template_id": template_id, "version": version, "content": content},
        )

    def trigger(self, change_type: str, change_id: str, change_content: dict[str, Any]):
        """Generic trigger for any change type (e.g., SLO violations)."""
        self._fire_async(change_type, change_id, change_content)

    def _fire_async(self, change_type: str, change_id: str, change_content: dict[str, Any]):
        thread = threading.Thread(
            target=self._run_gate,
            args=(change_type, change_id, change_content),
            daemon=True,
            name=f"gate-{change_id}",
        )
        thread.start()
        logger.info("Gate triggered async for %s %s", change_type, change_id)

    def _run_gate(self, change_type: str, change_id: str, change_content: dict[str, Any]):
        db = self._db_factory()
        try:
            # Distributed lock: only one replica runs the gate per change_id
            lock_name = f"gate_{change_id}"[:64]
            if not self._try_acquire(db, lock_name):
                logger.debug("Gate skipped (lock held by another instance): %s", change_id)
                return

            try:
                gate = RegressionGate(db=db, account=self._account)
                result = gate.validate_change(
                    change_type=ChangeType(change_type),
                    change_id=change_id,
                    change_content=change_content,
                )
                verdict = result.get("verdict", "unknown")
                logger.info("Gate result for %s: %s", change_id, verdict)
                if verdict == "fail":
                    logger.warning(
                        "GATE FAILED for %s — metrics: %s",
                        change_id, result.get("metrics"),
                    )
            finally:
                self._release(db, lock_name)

        except Exception as e:
            logger.error("Gate execution failed for %s: %s", change_id, e)
        finally:
            try:
                db.close()
            except Exception:
                pass

    def _try_acquire(self, db, lock_name: str) -> bool:
        """Try to acquire distributed lock. Returns True if acquired."""
        from sqlalchemy import text
        from api.models import DistributedLock
        import socket
        import os

        instance_id = f"{socket.gethostname()}:{os.getpid()}"
        now = datetime.now()
        expires_at = now + timedelta(seconds=_LOCK_TTL_SECONDS)

        # Fast path: INSERT (first writer wins)
        try:
            db.add(DistributedLock(
                lock_name=lock_name,
                instance_id=instance_id,
                acquired_at=now,
                expires_at=expires_at,
                task_name="gate",
            ))
            db.commit()
            return True
        except (IntegrityError, OperationalError):
            db.rollback()

        # Slow path: take over only if expired (previous gate crashed)
        result = db.execute(
            text(
                "UPDATE distributed_locks "
                "SET instance_id = :iid, acquired_at = :now, expires_at = :exp "
                "WHERE lock_name = :name AND expires_at < :now"
            ),
            {"iid": instance_id, "now": now, "exp": expires_at, "name": lock_name},
        )
        db.commit()
        return result.rowcount > 0

    @staticmethod
    def _release(db, lock_name: str):
        from sqlalchemy import text
        try:
            db.execute(
                text("DELETE FROM distributed_locks WHERE lock_name = :name"),
                {"name": lock_name},
            )
            db.commit()
        except Exception as e:
            logger.warning("Gate lock release failed: %s", e)
