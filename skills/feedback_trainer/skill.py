"""FeedbackTrainerSkill — train feedback classifier from labeled data.

Runs in isolated conda env (agent-engine-train) with PyTorch + transformers.
Produces ONNX model artifact registered in model_artifacts table.
"""

from __future__ import annotations

from pydantic import Field

from core.skills.base import (
    Skill, SkillInput, SkillOutput, SkillRequirement,
    RepoType, AccessScope, SideEffectCategory, SideEffectProfile,
)
from core.logging_config import get_logger

logger = get_logger(__name__)


class TrainerInput(SkillInput):
    dataset_path: str = Field(description="Path to training JSONL file")
    base_model: str = Field(default="bert-base-multilingual-cased")
    epochs: int = Field(default=5, ge=1, le=50)
    batch_size: int = Field(default=16, ge=1, le=128)
    learning_rate: float = Field(default=2e-5, gt=0)
    output_dir: str = Field(default="~/.mo-agent/models/feedback_classifier")


class TrainerOutput(SkillOutput):
    artifact_id: str | None = None
    model_path: str | None = None
    metrics: dict | None = None


LABELS = ["correction", "frustration", "rephrasing", "clarification", "positive", "neutral"]


class FeedbackTrainerSkill(Skill[TrainerInput, TrainerOutput]):
    """Train a feedback classifier and export to ONNX."""

    name = "feedback_trainer"
    version = "1.0.0"
    description = "Train feedback classification model from labeled conversation data"
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.READ, llm_required=False,
        gpu_required=True, conda_env="agent-engine-train", timeout_seconds=7200,
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.WRITE, external_apis=[], mock_strategy="skip",
    )

    def __init__(self, db=None) -> None:
        self._db = db

    def validate_input(self, input_data: dict) -> TrainerInput:
        return TrainerInput(**input_data)

    async def execute(self, input: TrainerInput) -> TrainerOutput:
        """Train model: load data → fine-tune → evaluate → export ONNX → register artifact."""
        from pathlib import Path
        import json

        output_dir = Path(input.output_dir).expanduser()
        output_dir.mkdir(parents=True, exist_ok=True)

        # 1. Load dataset
        samples = _load_jsonl(input.dataset_path)
        if len(samples) < 50:
            return TrainerOutput(success=False, result=None, error=f"Need ≥50 samples, got {len(samples)}")

        # 2. Split: 80/10/10
        n = len(samples)
        train_data = samples[:int(n * 0.8)]
        val_data = samples[int(n * 0.8):int(n * 0.9)]
        test_data = samples[int(n * 0.9):]

        # 3. Train
        metrics = _train(
            train_data, val_data, test_data,
            base_model=input.base_model,
            epochs=input.epochs,
            batch_size=input.batch_size,
            lr=input.learning_rate,
            output_dir=str(output_dir),
        )

        # 4. Export ONNX
        onnx_path = str(output_dir / "model.onnx")
        _export_onnx(str(output_dir), onnx_path, input.base_model)

        # 5. Register artifact
        artifact_id = None
        if self._db:
            from core.models.artifact_manager import ArtifactManager
            mgr = ArtifactManager(self._db)
            artifact_id = mgr.save(
                model_name="feedback_classifier",
                version=self.version,
                artifact_path=onnx_path,
                base_model=input.base_model,
                artifact_format="onnx",
                metrics=metrics,
                training_config={
                    "epochs": input.epochs, "batch_size": input.batch_size,
                    "lr": input.learning_rate,
                },
                dataset_size=len(samples),
                activate=True,
            )

        return TrainerOutput(
            success=True, result=metrics,
            artifact_id=artifact_id, model_path=onnx_path, metrics=metrics,
        )


def _load_jsonl(path: str) -> list[dict]:
    import json
    from pathlib import Path
    data = []
    with Path(path).open() as f:
        for i, line in enumerate(f, 1):
            line = line.strip()
            if line:
                try:
                    data.append(json.loads(line))
                except json.JSONDecodeError:
                    logger.warning(f"Skipping malformed JSON at line {i} in {path}")
    return data


def _train(
    train_data: list[dict], val_data: list[dict], test_data: list[dict],
    *, base_model: str, epochs: int, batch_size: int, lr: float, output_dir: str,
) -> dict:
    """Fine-tune BERT for 6-class feedback classification."""
    import torch
    from transformers import (
        AutoTokenizer, AutoModelForSequenceClassification,
        TrainingArguments, Trainer,
    )
    from datasets import Dataset

    label2id = {l: i for i, l in enumerate(LABELS)}

    def to_hf_dataset(samples: list[dict]) -> Dataset:
        texts, labels, weights = [], [], []
        for s in samples:
            text = f"{s.get('user_query', '')} [SEP] {s.get('agent_response', '')} [SEP] {s.get('followup', '')}"
            texts.append(text[:512])
            labels.append(label2id.get(s.get("label", "neutral"), 5))
            weights.append(s.get("weight", 1.0))
        return Dataset.from_dict({"text": texts, "label": labels, "weight": weights})

    tokenizer = AutoTokenizer.from_pretrained(base_model)
    model = AutoModelForSequenceClassification.from_pretrained(
        base_model, num_labels=len(LABELS), problem_type="single_label_classification",
    )

    train_ds = to_hf_dataset(train_data)
    val_ds = to_hf_dataset(val_data)
    test_ds = to_hf_dataset(test_data)

    def tokenize(batch):
        return tokenizer(batch["text"], padding="max_length", truncation=True, max_length=256)

    train_ds = train_ds.map(tokenize, batched=True, remove_columns=["text", "weight"])
    val_ds = val_ds.map(tokenize, batched=True, remove_columns=["text", "weight"])
    test_ds = test_ds.map(tokenize, batched=True, remove_columns=["text", "weight"])

    args = TrainingArguments(
        output_dir=output_dir,
        num_train_epochs=epochs,
        per_device_train_batch_size=batch_size,
        per_device_eval_batch_size=batch_size,
        learning_rate=lr,
        eval_strategy="epoch",
        save_strategy="epoch",
        load_best_model_at_end=True,
        metric_for_best_model="eval_loss",
        logging_steps=50,
        fp16=torch.cuda.is_available(),
    )

    trainer = Trainer(model=model, args=args, train_dataset=train_ds, eval_dataset=val_ds)
    trainer.train()

    # Evaluate on test set
    test_results = trainer.evaluate(test_ds)
    predictions = trainer.predict(test_ds)
    preds = predictions.predictions.argmax(-1)
    labels_true = predictions.label_ids

    from sklearn.metrics import accuracy_score, f1_score
    accuracy = accuracy_score(labels_true, preds)
    f1 = f1_score(labels_true, preds, average="weighted")

    # Save best model
    trainer.save_model(output_dir)
    tokenizer.save_pretrained(output_dir)

    return {
        "accuracy": round(accuracy, 4),
        "f1_weighted": round(f1, 4),
        "eval_loss": round(test_results.get("eval_loss", 0), 4),
        "train_samples": len(train_data),
        "val_samples": len(val_data),
        "test_samples": len(test_data),
    }


def _export_onnx(model_dir: str, onnx_path: str, base_model: str) -> None:
    """Export trained model to ONNX format for fast CPU inference."""
    from optimum.onnxruntime import ORTModelForSequenceClassification

    model = ORTModelForSequenceClassification.from_pretrained(model_dir, export=True)
    model.save_pretrained(onnx_path.replace("/model.onnx", ""))
