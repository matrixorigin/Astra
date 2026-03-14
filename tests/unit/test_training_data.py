"""Tests for training data pipeline."""

from unittest.mock import Mock, call

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
        # First call: fetch user-agent pairs (raw SQL)
        fetch_pairs = Mock()
        fetch_pairs.fetchall.return_value = [
            (
                "evt-1",
                "How do I implement a binary search tree?",
                "Here's how to implement a binary search tree:\n\n"
                "1. Define a Node class with value, left, right\n"
                "2. Implement insert method\n"
                "3. Implement search method\n\n"
                "```python\nclass Node:\n    def __init__(self, val):\n        self.val = val\n```\n"
                "This gives you a working BST with O(log n) operations.",
            ),
        ]
        db.execute.return_value = fetch_pairs

        # Contamination check now uses ORM query chain
        db.query.return_value.filter.return_value.order_by.return_value.limit.return_value.all.return_value = []

        pipeline = TrainingDataPipeline(lambda: db)
        examples = pipeline.extract_examples("sess-1", min_quality=DataQuality.SILVER)

        assert len(examples) > 0
        assert examples[0].session_id == "sess-1"

    def test_store_example_inserts_when_no_duplicate(self):
        db = _mock_db()
        # Mirrors ORM chain in store_example: db.query(TrainingData).filter_by(content_hash=...).first()
        db.query.return_value.filter_by.return_value.first.return_value = None

        pipeline = TrainingDataPipeline(lambda: db)
        example = TrainingExample(
            example_id="ex-1",
            session_id="sess-1",
            input_text="Question",
            output_text="Answer",
            quality=DataQuality.GOLD,
            contamination_score=0.1,
        )

        pipeline.store_example(example)
        db.add.assert_called_once()
        db.commit.assert_called_once()

    def test_store_example_skips_duplicate(self):
        db = _mock_db()
        db.query.return_value.filter_by.return_value.first.return_value = Mock()

        pipeline = TrainingDataPipeline(lambda: db)
        example = TrainingExample(
            example_id="ex-1",
            session_id="sess-1",
            input_text="Question",
            output_text="Answer",
            quality=DataQuality.GOLD,
            contamination_score=0.1,
        )

        pipeline.store_example(example)
        db.add.assert_not_called()
        db.commit.assert_not_called()

    def test_get_dataset(self):
        db = _mock_db()
        row1 = Mock(input_text="Input 1", output_text="Output 1", contamination_score=0.05)
        row2 = Mock(input_text="Input 2", output_text="Output 2", contamination_score=0.1)
        db.query.return_value.filter.return_value.order_by.return_value.limit.return_value.all.return_value = [
            row1,
            row2,
        ]

        pipeline = TrainingDataPipeline(lambda: db)
        dataset = pipeline.get_dataset(quality=DataQuality.GOLD, limit=100)

        assert len(dataset) == 2
        assert dataset[0]["input"] == "Input 1"
        assert dataset[0]["contamination"] == 0.05

    def test_get_statistics(self):
        db = _mock_db()
        # ORM query returns named-tuple-like rows: (quality, count, avg_contamination)
        row1 = ("gold", 100, 0.05)
        row2 = ("silver", 50, 0.15)
        db.query.return_value.group_by.return_value.all.return_value = [row1, row2]

        pipeline = TrainingDataPipeline(lambda: db)
        stats = pipeline.get_statistics()

        assert stats["total"] == 150
        assert stats["by_quality"]["gold"]["count"] == 100
        assert stats["by_quality"]["silver"]["avg_contamination"] == 0.15

    def test_assess_quality_rejects_empty(self):
        pipeline = TrainingDataPipeline(_mock_db())
        assert pipeline._assess_quality("Q", "") == DataQuality.REJECTED
        assert pipeline._assess_quality("Q", "short") == DataQuality.REJECTED

    def test_assess_quality_rejects_refusal(self):
        pipeline = TrainingDataPipeline(_mock_db())
        assert (
            pipeline._assess_quality("Q", "I can't help with that request.") == DataQuality.REJECTED
        )
        assert (
            pipeline._assess_quality("Q", "I'm sorry, but I can't assist with this.")
            == DataQuality.REJECTED
        )

    def test_assess_quality_structured_output_scores_higher(self):
        pipeline = TrainingDataPipeline(_mock_db())
        # Structured output with code, lists, and relevance should score well
        structured = (
            "Here's how to sort a list:\n\n"
            "1. Use the built-in sorted() function\n"
            "2. Or implement quicksort\n\n"
            "```python\ndef quicksort(arr):\n    if len(arr) <= 1: return arr\n```\n"
            "This gives you O(n log n) average performance."
        )
        quality = pipeline._assess_quality("How do I sort a list?", structured)
        assert quality in (DataQuality.GOLD, DataQuality.SILVER)

    def test_assess_quality_with_llm_judge(self):
        db = _mock_db()
        llm = Mock()
        llm.chat.return_value = Mock(content="4")

        pipeline = TrainingDataPipeline(lambda: db, llm_client=llm)
        quality = pipeline._assess_quality("What is X?", "X is a well-known concept...")
        assert quality == DataQuality.GOLD

    def test_contamination_detection(self):
        db = _mock_db()
        # Existing training data — ORM query chain
        db.query.return_value.filter.return_value.order_by.return_value.limit.return_value.all.return_value = [
            (
                "What is Python?",
                "Python is a programming language used for web development and data science.",
            ),
        ]

        pipeline = TrainingDataPipeline(lambda: db)
        # Near-duplicate should have high contamination
        score = pipeline._check_contamination(
            "other-session",
            "What is Python?",
            "Python is a programming language used for web development and data science.",
        )
        assert score > 0.5  # High overlap

    def test_contamination_clean(self):
        db = _mock_db()
        db.query.return_value.filter.return_value.order_by.return_value.limit.return_value.all.return_value = [
            ("What is Python?", "Python is a programming language."),
        ]

        pipeline = TrainingDataPipeline(lambda: db)
        # Completely different content should have low contamination
        score = pipeline._check_contamination(
            "other-session",
            "How does quantum computing work?",
            "Quantum computing uses qubits that can exist in superposition states.",
        )
        assert score < 0.3

    def test_content_hash_deterministic(self):
        pipeline = TrainingDataPipeline(_mock_db())
        h1 = pipeline._content_hash("hello", "world")
        h2 = pipeline._content_hash("hello", "world")
        h3 = pipeline._content_hash("Hello", "World")  # Case insensitive
        assert h1 == h2
        assert h1 == h3  # Normalized

    def test_ngram_extraction(self):
        pipeline = TrainingDataPipeline(_mock_db())
        ngrams = pipeline._extract_ngrams("the quick brown fox jumps", n=3)
        assert "the quick brown" in ngrams
        assert "quick brown fox" in ngrams
        assert len(ngrams) == 3
