"""Model artifact management — save, load, activate trained models."""

from __future__ import annotations

from datetime import datetime, timezone

from sqlalchemy import text
from uuid_utils import uuid7

from core.logging_config import get_logger
from core.db_consumer import DbConsumer, DbFactory

logger = get_logger(__name__)


class ArtifactManager(DbConsumer):
    """Manage trained model artifacts with versioning."""

    def __init__(self, db_factory: DbFactory) -> None:
        super().__init__(db_factory)

    def save(
        self,
        model_name: str,
        version: str,
        artifact_path: str,
        *,
        base_model: str | None = None,
        artifact_format: str = "onnx",
        metrics: dict | None = None,
        training_config: dict | None = None,
        dataset_size: int | None = None,
        created_by: str | None = None,
        activate: bool = False,
    ) -> str:
        """Save a new model artifact. Optionally activate it."""
        with self._db() as db:
            artifact_id = str(uuid7())
            db.execute(text("""
                INSERT INTO model_artifacts
                    (artifact_id, model_name, version, base_model, artifact_path,
                     artifact_format, metrics, training_config, dataset_size,
                     is_active, created_by)
                VALUES (:aid, :name, :ver, :base, :path, :fmt, :metrics, :cfg, :ds, :active, :by)
            """), {
                "aid": artifact_id, "name": model_name, "ver": version,
                "base": base_model, "path": artifact_path, "fmt": artifact_format,
                "metrics": _json_or_none(metrics), "cfg": _json_or_none(training_config),
                "ds": dataset_size, "active": 1 if activate else 0, "by": created_by,
            })
            if activate:
                self._deactivate_others(model_name, artifact_id)
            db.commit()
            logger.info(f"Saved artifact {model_name}@{version} ({artifact_id}), active={activate}")
            return artifact_id

    def activate(self, artifact_id: str) -> bool:
        """Activate a specific artifact (deactivates others of same model_name)."""
        with self._db() as db:
            row = db.execute(text(
                "SELECT model_name FROM model_artifacts WHERE artifact_id = :aid"
            ), {"aid": artifact_id}).fetchone()
            if not row:
                return False
            model_name = row[0]
            db.execute(text(
                "UPDATE model_artifacts SET is_active = 0 WHERE model_name = :name"
            ), {"name": model_name})
            db.execute(text(
                "UPDATE model_artifacts SET is_active = 1 WHERE artifact_id = :aid"
            ), {"aid": artifact_id})
            db.commit()
            return True

    def get_active(self, model_name: str) -> dict | None:
        """Get the currently active artifact for a model."""
        with self._db() as db:
            row = db.execute(text("""
                SELECT artifact_id, version, artifact_path, artifact_format, metrics
                FROM model_artifacts
                WHERE model_name = :name AND is_active = 1
                LIMIT 1
            """), {"name": model_name}).fetchone()
            if not row:
                return None
            return {
                "artifact_id": row[0], "version": row[1], "artifact_path": row[2],
                "artifact_format": row[3], "metrics": row[4],
            }

    def list_versions(self, model_name: str) -> list[dict]:
        """List all versions of a model, newest first."""
        with self._db() as db:
            rows = db.execute(text("""
                SELECT artifact_id, version, is_active, metrics, dataset_size, created_at
                FROM model_artifacts
                WHERE model_name = :name
                ORDER BY created_at DESC
            """), {"name": model_name}).fetchall()
            return [
                {"artifact_id": r[0], "version": r[1], "is_active": bool(r[2]),
                 "metrics": r[3], "dataset_size": r[4], "created_at": str(r[5])}
                for r in rows
            ]

    def _deactivate_others(self, model_name: str, keep_id: str) -> None:
        with self._db() as db:
            db.execute(text(
                "UPDATE model_artifacts SET is_active = 0 WHERE model_name = :name AND artifact_id != :aid"
            ), {"name": model_name, "aid": keep_id})


def _json_or_none(val: dict | None) -> str | None:
    if val is None:
        return None
    import json
    return json.dumps(val)
