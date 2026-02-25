"""Tests for ModelArtifact table model and ArtifactManager."""

import json
import pytest
from unittest.mock import MagicMock, call

from core.models.artifact_manager import ArtifactManager, _json_or_none


class TestJsonOrNone:

    def test_none_returns_none(self):
        assert _json_or_none(None) is None

    def test_dict_returns_json_string(self):
        result = _json_or_none({"accuracy": 0.87})
        assert json.loads(result) == {"accuracy": 0.87}

    def test_empty_dict(self):
        result = _json_or_none({})
        assert result == "{}"


class TestArtifactManager:

    def _make_manager(self):
        db = MagicMock()
        return ArtifactManager(lambda: db), db

    def test_save_without_activate(self):
        mgr, db = self._make_manager()
        aid = mgr.save("feedback_classifier", "1.0.0", "/models/fc_v1.onnx")
        assert len(aid) == 36  # uuid7
        # INSERT called, no deactivate
        assert db.execute.call_count == 1
        db.commit.assert_called_once()

    def test_save_with_activate_deactivates_others(self):
        mgr, db = self._make_manager()
        aid = mgr.save("feedback_classifier", "1.0.0", "/models/fc_v1.onnx", activate=True)
        # INSERT + deactivate others = 2 execute calls
        assert db.execute.call_count == 2
        db.commit.assert_called_once()

    def test_activate_existing(self):
        mgr, db = self._make_manager()
        db.execute.return_value.fetchone.return_value = ("feedback_classifier",)
        result = mgr.activate("some-id")
        assert result is True
        # SELECT + deactivate all + activate one = 3 calls
        assert db.execute.call_count == 3
        db.commit.assert_called_once()

    def test_activate_nonexistent(self):
        mgr, db = self._make_manager()
        db.execute.return_value.fetchone.return_value = None
        result = mgr.activate("nonexistent")
        assert result is False
        db.commit.assert_not_called()

    def test_get_active_found(self):
        mgr, db = self._make_manager()
        db.execute.return_value.fetchone.return_value = (
            "aid-1", "1.0.0", "/models/fc.onnx", "onnx", {"accuracy": 0.87},
        )
        result = mgr.get_active("feedback_classifier")
        assert result["version"] == "1.0.0"
        assert result["artifact_path"] == "/models/fc.onnx"

    def test_get_active_not_found(self):
        mgr, db = self._make_manager()
        db.execute.return_value.fetchone.return_value = None
        assert mgr.get_active("nonexistent") is None

    def test_list_versions(self):
        mgr, db = self._make_manager()
        db.execute.return_value.fetchall.return_value = [
            ("aid-2", "2.0.0", 1, {"accuracy": 0.90}, 2000, "2026-02-21"),
            ("aid-1", "1.0.0", 0, {"accuracy": 0.87}, 1000, "2026-02-20"),
        ]
        versions = mgr.list_versions("feedback_classifier")
        assert len(versions) == 2
        assert versions[0]["is_active"] is True
        assert versions[1]["is_active"] is False


class TestModelArtifactTable:

    def test_model_class_exists(self):
        from api.models import ModelArtifact
        assert ModelArtifact.__tablename__ == "model_artifacts"

    def test_required_columns(self):
        from api.models import ModelArtifact
        columns = {c.name for c in ModelArtifact.__table__.columns}
        expected = {"artifact_id", "model_name", "version", "artifact_path",
                    "artifact_format", "is_active", "metrics", "created_at"}
        assert expected.issubset(columns)
