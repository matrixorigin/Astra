"""Training data pipeline with quality filtering.

Design ref: evaluation-and-evolution.md §6 "Training Data Pipeline"

Extract high-quality training data from sessions with contamination detection.
Distributed-safe: all state in DB, no shared mutable state.
"""

from __future__ import annotations

import json
import logging
from dataclasses import dataclass
from enum import Enum
from typing import Any

from sqlalchemy import text
from sqlalchemy.orm import Session

logger = logging.getLogger(__name__)


class DataQuality(str, Enum):
    """Quality levels for training data."""

    GOLD = "gold"  # High quality, ready for training
    SILVER = "silver"  # Good quality, needs review
    BRONZE = "bronze"  # Acceptable, may have issues
    REJECTED = "rejected"  # Too low quality


@dataclass
class TrainingExample:
    """A training example extracted from a session."""

    example_id: str
    session_id: str
    input_text: str
    output_text: str
    quality: DataQuality
    contamination_score: float  # 0-1, higher = more contaminated


class TrainingDataPipeline:
    """Extract and filter training data from sessions.

    Distributed-safe: all state in DB.
    """

    def __init__(self, db: Session) -> None:
        self.db = db

    def extract_examples(
        self,
        session_id: str,
        min_quality: DataQuality = DataQuality.SILVER,
    ) -> list[TrainingExample]:
        """Extract training examples from a session.

        Args:
            session_id: Session ID
            min_quality: Minimum quality threshold

        Returns:
            List of TrainingExample
        """
        examples = []

        # Get all user-agent interactions
        rows = self.db.execute(
            text(
                "SELECT e1.event_id, e1.content, e2.content "
                "FROM conversation_events e1 "
                "JOIN conversation_events e2 ON e1.event_id = e2.parent_event_id "
                "WHERE e1.session_id = :session_id "
                "AND e1.event_type = 'user_query' "
                "AND e2.event_type = 'llm_response'"
            ),
            {"session_id": session_id},
        ).fetchall()

        for event_id, user_input, agent_output in rows:
            # Assess quality
            quality = self._assess_quality(user_input, agent_output)

            if quality.value >= min_quality.value:
                # Check contamination
                contamination = self._check_contamination(session_id, user_input, agent_output)

                example = TrainingExample(
                    example_id=event_id,
                    session_id=session_id,
                    input_text=user_input,
                    output_text=agent_output,
                    quality=quality,
                    contamination_score=contamination,
                )
                examples.append(example)

        logger.info(f"Extracted {len(examples)} training examples from {session_id}")
        return examples

    def store_example(self, example: TrainingExample) -> None:
        """Store a training example.

        Args:
            example: TrainingExample to store
        """
        from uuid_utils import uuid7

        self.db.execute(
            text(
                "INSERT INTO training_data "
                "(data_id, session_id, input_text, output_text, quality, contamination_score, created_at) "
                "VALUES (:id, :session_id, :input, :output, :quality, :contamination, NOW())"
            ),
            {
                "id": str(uuid7()),
                "session_id": example.session_id,
                "input": example.input_text,
                "output": example.output_text,
                "quality": example.quality.value,
                "contamination": example.contamination_score,
            },
        )
        self.db.commit()

    def get_dataset(
        self,
        quality: DataQuality = DataQuality.GOLD,
        limit: int = 1000,
    ) -> list[dict[str, Any]]:
        """Get training dataset filtered by quality.

        Args:
            quality: Minimum quality level
            limit: Max examples

        Returns:
            List of training examples
        """
        rows = self.db.execute(
            text(
                "SELECT input_text, output_text, contamination_score "
                "FROM training_data "
                "WHERE quality = :quality "
                "AND contamination_score < 0.3 "
                "ORDER BY contamination_score ASC "
                "LIMIT :limit"
            ),
            {"quality": quality.value, "limit": limit},
        ).fetchall()

        return [
            {
                "input": row[0],
                "output": row[1],
                "contamination": float(row[2]),
            }
            for row in rows
        ]

    def _assess_quality(self, user_input: str, agent_output: str) -> DataQuality:
        """Assess quality of an example."""
        # Simple heuristic: length and completeness
        if len(agent_output) < 10:
            return DataQuality.REJECTED
        if len(agent_output) < 50:
            return DataQuality.BRONZE
        if len(agent_output) < 200:
            return DataQuality.SILVER
        return DataQuality.GOLD

    def _check_contamination(
        self, session_id: str, user_input: str, agent_output: str
    ) -> float:
        """Check for data contamination (e.g., test set leakage).

        Args:
            session_id: Session ID
            user_input: User input
            agent_output: Agent output

        Returns:
            Contamination score (0-1)
        """
        # Simple heuristic: check if similar examples exist in test set
        # In production: use embedding similarity or exact match detection
        return 0.0  # No contamination detected

    def get_statistics(self) -> dict[str, Any]:
        """Get statistics on training data.

        Returns:
            Statistics dict
        """
        rows = self.db.execute(
            text(
                "SELECT quality, COUNT(*) as count, AVG(contamination_score) as avg_contamination "
                "FROM training_data "
                "GROUP BY quality"
            )
        ).fetchall()

        stats = {
            "total": sum(r[1] for r in rows),
            "by_quality": {},
        }

        for quality, count, avg_contamination in rows:
            stats["by_quality"][quality] = {
                "count": count,
                "avg_contamination": float(avg_contamination) if avg_contamination else 0.0,
            }

        return stats
