"""Tests for FeedbackTrainerSkill and FeedbackClassifierSkill."""

import json
import pytest
from unittest.mock import MagicMock

from skills.feedback_trainer.skill import (
    FeedbackTrainerSkill, TrainerInput, TrainerOutput, _load_jsonl, LABELS,
)
from skills.feedback_classifier.skill import (
    FeedbackClassifierSkill, ClassifierInput, ClassifierOutput,
)


# ---------------------------------------------------------------------------
# Trainer
# ---------------------------------------------------------------------------

class TestTrainerInput:

    def test_defaults(self):
        inp = TrainerInput(dataset_path="/data/train.jsonl")
        assert inp.base_model == "bert-base-multilingual-cased"
        assert inp.epochs == 5
        assert inp.batch_size == 16

    def test_validation_epochs_range(self):
        with pytest.raises(Exception):
            TrainerInput(dataset_path="/x", epochs=0)

    def test_validation_lr_positive(self):
        with pytest.raises(Exception):
            TrainerInput(dataset_path="/x", learning_rate=-1)


class TestTrainerSkill:

    def test_requirements(self):
        skill = FeedbackTrainerSkill()
        assert skill.requirements.gpu_required is True
        assert skill.requirements.conda_env == "agent-engine-train"
        assert skill.requirements.timeout_seconds == 7200

    def test_validate_input(self):
        skill = FeedbackTrainerSkill()
        inp = skill.validate_input({"dataset_path": "/data/train.jsonl", "epochs": 3})
        assert inp.epochs == 3

    @pytest.mark.asyncio
    async def test_too_few_samples(self, tmp_path):
        """Should fail gracefully with <50 samples."""
        jsonl = tmp_path / "small.jsonl"
        jsonl.write_text("\n".join(
            json.dumps({"user_query": "q", "agent_response": "a", "followup": "f", "label": "neutral"})
            for _ in range(10)
        ))
        skill = FeedbackTrainerSkill()
        inp = TrainerInput(dataset_path=str(jsonl))
        result = await skill.execute(inp)
        assert result.success is False
        assert "50" in result.error


class TestLoadJsonl:

    def test_load(self, tmp_path):
        f = tmp_path / "data.jsonl"
        f.write_text('{"a":1}\n{"b":2}\n\n')
        data = _load_jsonl(str(f))
        assert len(data) == 2
        assert data[0] == {"a": 1}

    def test_empty_file(self, tmp_path):
        f = tmp_path / "empty.jsonl"
        f.write_text("")
        assert _load_jsonl(str(f)) == []

    def test_malformed_line_skipped(self, tmp_path):
        """Malformed JSON lines should be skipped, not crash."""
        f = tmp_path / "bad.jsonl"
        f.write_text('{"a":1}\nnot json\n{"b":2}\n')
        data = _load_jsonl(str(f))
        assert len(data) == 2


class TestLabels:

    def test_six_labels(self):
        assert len(LABELS) == 6
        assert "correction" in LABELS
        assert "neutral" in LABELS


# ---------------------------------------------------------------------------
# Classifier
# ---------------------------------------------------------------------------

class TestClassifierInput:

    def test_required_fields(self):
        inp = ClassifierInput(user_query="q", agent_response="a", followup="f")
        assert inp.user_query == "q"


class TestClassifierSkill:

    def test_requirements_lightweight(self):
        """Classifier should be lightweight (no GPU, no conda)."""
        skill = FeedbackClassifierSkill()
        assert skill.requirements.gpu_required is False
        assert skill.requirements.conda_env is None
        assert skill.requirements.timeout_seconds == 10

    def test_validate_input(self):
        skill = FeedbackClassifierSkill()
        inp = skill.validate_input({"user_query": "q", "agent_response": "a", "followup": "f"})
        assert isinstance(inp, ClassifierInput)

    @pytest.mark.asyncio
    async def test_heuristic_fallback_no_db(self):
        """Without DB, should fall back to heuristic."""
        skill = FeedbackClassifierSkill(db=None)
        inp = ClassifierInput(user_query="how to sort?", agent_response="use sorted()", followup="不对，我要降序")
        result = await skill.execute(inp)
        assert result.success is True
        assert result.signal_type in LABELS
        assert "heuristic" in result.reasoning

    @pytest.mark.asyncio
    async def test_heuristic_fallback_no_model(self):
        """With DB but no active model, should fall back to heuristic."""
        db = MagicMock()
        db.execute.return_value.fetchone.return_value = None
        skill = FeedbackClassifierSkill(db=db)
        inp = ClassifierInput(user_query="q", agent_response="a", followup="thanks!")
        result = await skill.execute(inp)
        assert result.success is True
        assert "heuristic" in result.reasoning

    @pytest.mark.asyncio
    async def test_positive_detection(self):
        """Heuristic should detect positive feedback."""
        skill = FeedbackClassifierSkill()
        inp = ClassifierInput(user_query="q", agent_response="a", followup="谢谢，完美！")
        result = await skill.execute(inp)
        assert result.signal_type == "positive"

    @pytest.mark.asyncio
    async def test_neutral_detection(self):
        """Unrelated followup should be neutral."""
        skill = FeedbackClassifierSkill()
        inp = ClassifierInput(user_query="q", agent_response="a", followup="另外一个问题，Python怎么读文件？")
        result = await skill.execute(inp)
        assert result.signal_type == "neutral"

    def test_ensure_model_no_db(self):
        skill = FeedbackClassifierSkill(db=None)
        assert skill._ensure_model() is False

    def test_ensure_model_import_error(self):
        """If onnxruntime not installed, should return False not crash."""
        db = MagicMock()
        db.execute.return_value.fetchone.return_value = (
            "aid-1", "1.0.0", "/nonexistent/model.onnx", "onnx", None,
        )
        skill = FeedbackClassifierSkill(db=db)
        # Will fail because path doesn't exist, but should not raise
        assert skill._ensure_model() is False
