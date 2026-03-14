"""Training data pipeline with quality filtering.

Design ref: evaluation-and-evolution.md §6 "Training Data Pipeline"

Extract high-quality training data from sessions with:
- Multi-signal quality assessment (not just length)
- Contamination detection via n-gram overlap with existing training data
- Deduplication via content hashing

Distributed-safe: all state in DB, no shared mutable state.
"""

from __future__ import annotations

import hashlib
import logging
import re
from collections import Counter
from dataclasses import dataclass
from enum import Enum
from typing import Any

from sqlalchemy import text
from core.db_consumer import DbConsumer, DbFactory

logger = logging.getLogger(__name__)


class DataQuality(str, Enum):
    """Quality levels for training data."""

    GOLD = "gold"
    SILVER = "silver"
    BRONZE = "bronze"
    REJECTED = "rejected"


@dataclass
class TrainingExample:
    """A training example extracted from a session."""

    example_id: str
    session_id: str
    input_text: str
    output_text: str
    quality: DataQuality
    contamination_score: float  # 0-1, higher = more contaminated


class TrainingDataPipeline(DbConsumer):
    """Extract and filter training data from sessions.

    Distributed-safe: all state in DB.
    """

    def __init__(self, db_factory: DbFactory, llm_client=None) -> None:
        super().__init__(db_factory)
        self.llm_client = llm_client

    def extract_examples(
        self,
        session_id: str,
        min_quality: DataQuality = DataQuality.SILVER,
    ) -> list[TrainingExample]:
        """Extract training examples from a session."""
        with self._db() as db:
            examples = []

            rows = db.execute(
                text(
                    "SELECT e1.event_id, e1.content, e2.content "
                    "FROM agent_events e1 "
                    "JOIN agent_events e2 ON e1.event_id = e2.parent_event_id "
                    "WHERE e1.session_id = :session_id "
                    "AND e1.event_type = 'user_query' "
                    "AND e2.event_type = 'llm_response'"
                ),
                {"session_id": session_id},
            ).fetchall()

            quality_order = {
                DataQuality.GOLD: 3,
                DataQuality.SILVER: 2,
                DataQuality.BRONZE: 1,
                DataQuality.REJECTED: 0,
            }
            min_order = quality_order[min_quality]

            for event_id, user_input, agent_output in rows:
                quality = self._assess_quality(user_input, agent_output)

                if quality_order[quality] >= min_order:
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
        """Store a training example (with dedup by content hash)."""
        with self._db() as db:
            from api.models import TrainingData
            from uuid_utils import uuid7

            content_hash = self._content_hash(example.input_text, example.output_text)

            existing = db.query(TrainingData).filter_by(content_hash=content_hash).first()
            if existing:
                logger.debug(f"Skipping duplicate: {content_hash[:16]}")
                return

            db.add(
                TrainingData(
                    data_id=str(uuid7()),
                    session_id=example.session_id,
                    input_text=example.input_text,
                    output_text=example.output_text,
                    quality=example.quality.value,
                    contamination_score=example.contamination_score,
                    content_hash=content_hash,
                )
            )
            db.commit()

    def get_dataset(
        self,
        quality: DataQuality = DataQuality.GOLD,
        limit: int = 1000,
    ) -> list[dict[str, Any]]:
        """Get training dataset filtered by quality."""
        with self._db() as db:
            from api.models import TrainingData

            rows = (
                db.query(TrainingData)
                .filter(
                    TrainingData.quality == quality.value,
                    TrainingData.contamination_score < 0.3,
                )
                .order_by(TrainingData.contamination_score.asc())
                .limit(limit)
                .all()
            )

            return [
                {
                    "input": r.input_text,
                    "output": r.output_text,
                    "contamination": float(r.contamination_score),
                }
                for r in rows
            ]

    def get_statistics(self) -> dict[str, Any]:
        """Get statistics on training data."""
        with self._db() as db:
            from sqlalchemy import func as sa_func
            from api.models import TrainingData

            rows = (
                db.query(
                    TrainingData.quality,
                    sa_func.count().label("count"),
                    sa_func.avg(TrainingData.contamination_score).label("avg_contamination"),
                )
                .group_by(TrainingData.quality)
                .all()
            )

            stats: dict[str, Any] = {"total": sum(r[1] for r in rows), "by_quality": {}}
            for quality, count, avg_contamination in rows:
                stats["by_quality"][quality] = {
                    "count": count,
                    "avg_contamination": float(avg_contamination) if avg_contamination else 0.0,
                }
            return stats

    # ── Quality Assessment ──────────────────────────────────────────

    def _assess_quality(self, user_input: str, agent_output: str) -> DataQuality:
        """Multi-signal quality assessment.

        Signals:
        1. Length — too short is low quality, but length alone doesn't mean high quality
        2. Completeness — does the output address the input?
        3. Structure — code blocks, lists, paragraphs indicate effort
        4. Refusal detection — "I can't help with that" is not training-worthy
        5. LLM-as-judge (if llm_client available) — authoritative quality score
        """
        # Fast reject: empty or near-empty
        if not agent_output or len(agent_output.strip()) < 10:
            return DataQuality.REJECTED

        # Fast reject: refusal patterns
        if self._is_refusal(agent_output):
            return DataQuality.REJECTED

        # If LLM available, use LLM-as-judge for authoritative scoring
        if self.llm_client:
            return self._llm_judge_quality(user_input, agent_output)

        # Heuristic scoring (no LLM fallback)
        score = self._heuristic_quality_score(user_input, agent_output)

        if score >= 0.7:
            return DataQuality.GOLD
        if score >= 0.4:
            return DataQuality.SILVER
        if score >= 0.2:
            return DataQuality.BRONZE
        return DataQuality.REJECTED

    def _is_refusal(self, text: str) -> bool:
        """Detect refusal/non-answer responses."""
        lower = text.lower().strip()
        refusal_starts = [
            "i can't",
            "i cannot",
            "i'm unable",
            "i am unable",
            "i'm sorry, but i can't",
            "as an ai",
            "i don't have",
            "i do not have",
        ]
        return any(lower.startswith(r) for r in refusal_starts)

    def _heuristic_quality_score(self, user_input: str, agent_output: str) -> float:
        """Score 0-1 based on multiple heuristic signals."""
        score = 0.0

        # Signal 1: Length (diminishing returns)
        length = len(agent_output)
        if length >= 50:
            score += 0.15
        if length >= 200:
            score += 0.1
        if length >= 500:
            score += 0.05

        # Signal 2: Structure (code blocks, lists, headers)
        if "```" in agent_output:
            score += 0.15  # Contains code
        if re.search(r"^\s*[-*]\s", agent_output, re.MULTILINE):
            score += 0.1  # Contains lists
        if re.search(r"^\s*\d+\.\s", agent_output, re.MULTILINE):
            score += 0.1  # Contains numbered steps

        # Signal 3: Relevance — keyword overlap between input and output
        input_words = set(user_input.lower().split())
        output_words = set(agent_output.lower().split())
        if input_words:
            overlap = len(input_words & output_words) / len(input_words)
            score += min(overlap * 0.2, 0.2)

        # Signal 4: Not repetitive
        sentences = [s.strip() for s in agent_output.split(".") if s.strip()]
        if len(sentences) > 1:
            unique_ratio = len(set(sentences)) / len(sentences)
            score += unique_ratio * 0.15

        return min(score, 1.0)

    def _llm_judge_quality(self, user_input: str, agent_output: str) -> DataQuality:
        """Use LLM to judge quality. Returns quality level."""
        prompt = (
            "Rate the quality of this AI response on a scale of 1-5.\n"
            "1=terrible/wrong, 2=poor, 3=acceptable, 4=good, 5=excellent.\n"
            "Reply with ONLY a single digit.\n\n"
            f"User question: {user_input[:500]}\n\n"
            f"AI response: {agent_output[:1000]}"
        )
        try:
            response = self.llm_client.chat(
                messages=[{"role": "user", "content": prompt}],
                user_id="training_data_pipeline",
                temperature=0.0,
                task_hint="training_data",
            )
            text = (response.content or "").strip()
            # Extract first digit
            match = re.search(r"[1-5]", text)
            if match:
                rating = int(match.group(0))
                if rating >= 4:
                    return DataQuality.GOLD
                if rating >= 3:
                    return DataQuality.SILVER
                if rating >= 2:
                    return DataQuality.BRONZE
            return DataQuality.REJECTED
        except Exception as e:
            logger.warning(f"LLM judge failed, falling back to heuristic: {e}")
            score = self._heuristic_quality_score(user_input, agent_output)
            if score >= 0.7:
                return DataQuality.GOLD
            if score >= 0.4:
                return DataQuality.SILVER
            if score >= 0.2:
                return DataQuality.BRONZE
            return DataQuality.REJECTED

    # ── Contamination Detection ─────────────────────────────────────

    def _check_contamination(self, session_id: str, user_input: str, agent_output: str) -> float:
        """Check for data contamination via n-gram overlap with existing training data.

        Compares against stored training examples to detect near-duplicates.
        Returns 0.0 (clean) to 1.0 (highly contaminated).
        """
        # Get recent training data for comparison
        with self._db() as db:
            from api.models import TrainingData

            rows = (
                db.query(TrainingData.input_text, TrainingData.output_text)
                .filter(TrainingData.session_id != session_id)
                .order_by(TrainingData.created_at.desc())
                .limit(100)
                .all()
            )

            if not rows:
                return 0.0

            # Build n-gram set from current example
            current_ngrams = self._extract_ngrams(user_input + " " + agent_output, n=3)
            if not current_ngrams:
                return 0.0

            # Check overlap with each stored example
            max_overlap = 0.0
            for stored_input, stored_output in rows:
                stored_ngrams = self._extract_ngrams(stored_input + " " + stored_output, n=3)
                if not stored_ngrams:
                    continue
                overlap = len(current_ngrams & stored_ngrams) / len(current_ngrams)
                max_overlap = max(max_overlap, overlap)

            return round(max_overlap, 3)

    def _extract_ngrams(self, text: str, n: int = 3) -> set[str]:
        """Extract word-level n-grams from text."""
        words = text.lower().split()
        if len(words) < n:
            return set()
        return {" ".join(words[i : i + n]) for i in range(len(words) - n + 1)}

    def _content_hash(self, input_text: str, output_text: str) -> str:
        """SHA-256 hash of normalized content for dedup."""
        normalized = input_text.strip().lower() + "|||" + output_text.strip().lower()
        return hashlib.sha256(normalized.encode()).hexdigest()
