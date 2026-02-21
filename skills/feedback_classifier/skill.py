"""FeedbackClassifierSkill — classify implicit feedback from conversation triples.

Uses ONNX Runtime for fast CPU inference (<50ms).
Falls back to heuristic detector if model not available.
"""

from __future__ import annotations

from pydantic import Field

from core.skills.base import (
    Skill, SkillInput, SkillOutput, SkillRequirement,
    RepoType, AccessScope, SideEffectCategory, SideEffectProfile,
)
from core.logging_config import get_logger

logger = get_logger(__name__)

LABELS = ["correction", "frustration", "rephrasing", "clarification", "positive", "neutral"]


class ClassifierInput(SkillInput):
    user_query: str = Field(description="Original user question")
    agent_response: str = Field(description="Agent's response")
    followup: str = Field(description="User's follow-up message")


class ClassifierOutput(SkillOutput):
    signal_type: str = "neutral"
    confidence: float = 0.0
    reasoning: str = ""


class FeedbackClassifierSkill(Skill[ClassifierInput, ClassifierOutput]):
    """Classify implicit feedback using ONNX model with heuristic fallback."""

    name = "feedback_classifier"
    version = "1.0.0"
    description = "Classify implicit user feedback from conversation context"
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.READ, llm_required=False,
        timeout_seconds=10,
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.READ, external_apis=[],
    )

    def __init__(self, db=None) -> None:
        self._db = db
        self._session = None  # lazy loaded ONNX session
        self._tokenizer = None

    def validate_input(self, input_data: dict) -> ClassifierInput:
        return ClassifierInput(**input_data)

    async def execute(self, input: ClassifierInput) -> ClassifierOutput:
        """Classify feedback. Try ONNX model first, fallback to heuristic."""
        # Try model inference
        if self._ensure_model():
            return self._predict(input)

        # Fallback to heuristic
        return self._heuristic_fallback(input)

    def _ensure_model(self) -> bool:
        """Lazy-load ONNX model from active artifact. Returns True if ready."""
        if self._session is not None:
            return True
        if not self._db:
            return False
        try:
            from core.models.artifact_manager import ArtifactManager
            artifact = ArtifactManager(self._db).get_active("feedback_classifier")
            if not artifact:
                return False

            from pathlib import Path
            model_dir = str(Path(artifact["artifact_path"]).parent)

            import onnxruntime as ort
            from transformers import AutoTokenizer

            self._session = ort.InferenceSession(artifact["artifact_path"])
            self._tokenizer = AutoTokenizer.from_pretrained(model_dir)
            return True
        except Exception as e:
            logger.debug(f"Failed to load ONNX model, using heuristic fallback: {e}")
            return False

    def _predict(self, input: ClassifierInput) -> ClassifierOutput:
        """Run ONNX inference."""
        import numpy as np

        text = f"{input.user_query} [SEP] {input.agent_response} [SEP] {input.followup}"
        tokens = self._tokenizer(
            text[:512], padding="max_length", truncation=True,
            max_length=256, return_tensors="np",
        )
        outputs = self._session.run(None, {
            "input_ids": tokens["input_ids"],
            "attention_mask": tokens["attention_mask"],
        })
        logits = outputs[0][0]

        # Softmax
        exp = np.exp(logits - np.max(logits))
        probs = exp / exp.sum()

        pred_idx = int(np.argmax(probs))
        signal_type = LABELS[pred_idx]
        confidence = float(probs[pred_idx])

        return ClassifierOutput(
            success=True, result={"signal_type": signal_type, "confidence": confidence},
            signal_type=signal_type, confidence=confidence,
            reasoning=f"model prediction (top: {signal_type}={confidence:.2f})",
        )

    @staticmethod
    def _heuristic_fallback(input: ClassifierInput) -> ClassifierOutput:
        """Use ImplicitFeedbackDetector as fallback."""
        from core.context.implicit_feedback import ImplicitFeedbackDetector
        signal = ImplicitFeedbackDetector.detect(input.followup, input.agent_response)
        return ClassifierOutput(
            success=True, result={"signal_type": signal.signal_type, "confidence": signal.confidence},
            signal_type=signal.signal_type, confidence=signal.confidence,
            reasoning=f"heuristic fallback: {signal.evidence}",
        )
