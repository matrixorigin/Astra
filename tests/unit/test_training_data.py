"""Tests for training data pipeline."""

from unittest.mock import Mock

import pytest

from core.evaluation.training_data import DataQuality, TrainingDataPipeline, TrainingExample


def _mock_db():
    return Mock()


class TestDataQuality:
    def test_quality_values(self):
        assert DataQuality.GOLD.value == "gold"
        assert DataQuality.SILVER.value == "silver"
        assert DataQuality.BRONZE.value == "bronze"
        assert DataQuality.REJECTED.value == "rejected"


class TestTrainingDataPipeline:
    def test_extract_examples(self):
        db = _mock_db()
        mock_execute = Mock()
        mock_execute.fetchall.return_value = [
            ("evt-1", "What is X?", "X is a concept that is very important and has many applications in various fields"),
        ]
        db.execute.return_value = mock_execute

        pipeline = TrainingDataPipeline(db)
        examples = pipeline.extract_examples("sess-1", min_quality=DataQuality.SILVER)

        assert len(examples) > 0
        assert examples[0].session_id == "sess-1"

    def test_store_example(self):
        db = _mock_db()
        pipeline = TrainingDataPipeline(db)

        example = TrainingExample(
            example_id="ex-1",
            session_id="sess-1",
            input_text="Question",
            output_text="Answer",
            quality=DataQuality.GOLD,
            contamination_score=0.1,
        )

        pipeline.store_example(example)
        db.execute.assert_called_once()
        db.commit.assert_called_once()

    def test_get_dataset(self):
        db = _mock_db()
        db.execute.return_value = Mock(
            fetchall=Mock(
                return_value=[
                    ("Input 1", "Output 1", 0.05),
                    ("Input 2", "Output 2", 0.1),
                ]
            )
        )

        pipeline = TrainingDataPipeline(db)
        dataset = pipeline.get_dataset(quality=DataQuality.GOLD, limit=100)

        assert len(dataset) == 2
        assert dataset[0]["input"] == "Input 1"
        assert dataset[0]["contamination"] == 0.05

    def test_get_statistics(self):
        db = _mock_db()
        db.execute.return_value = Mock(
            fetchall=Mock(
                return_value=[
                    ("gold", 100, 0.05),
                    ("silver", 50, 0.15),
                ]
            )
        )

        pipeline = TrainingDataPipeline(db)
        stats = pipeline.get_statistics()

        assert stats["total"] == 150
        assert stats["by_quality"]["gold"]["count"] == 100
        assert stats["by_quality"]["silver"]["avg_contamination"] == 0.15

    def test_assess_quality(self):
        db = _mock_db()
        pipeline = TrainingDataPipeline(db)

        assert pipeline._assess_quality("Q", "A") == DataQuality.REJECTED
        assert pipeline._assess_quality("Q", "A" * 30) == DataQuality.BRONZE
        assert pipeline._assess_quality("Q", "A" * 100) == DataQuality.SILVER
        assert pipeline._assess_quality("Q", "A" * 300) == DataQuality.GOLD
