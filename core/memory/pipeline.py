"""Unified memory pipeline: Observer → Reflector → PollutionDetector.

Provides a single entry point to run the full memory lifecycle for a user,
instead of relying on separate governance tasks.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from sqlalchemy.orm import Session

from core.logging_config import get_logger

logger = get_logger(__name__)


@dataclass
class PipelineResult:
    observations_extracted: int = 0
    reflections_condensed: int = 0
    contradictions_found: int = 0
    quarantined: int = 0
    regression_signals: int = 0
    errors: list[str] = field(default_factory=list)


def run_memory_pipeline(
    db: Session,
    user_id: str,
    session_id: str | None = None,
    llm_client=None,
    observe_threshold: int | None = None,
) -> PipelineResult:
    """Run full memory pipeline: observe → reflect → detect pollution.

    Args:
        db: Database session
        user_id: Target user
        session_id: Optional session scope (None = all sessions)
        llm_client: LLM client for Observer/Reflector
        observe_threshold: Token threshold for observer (None = default)
    """
    result = PipelineResult()

    # Phase 1: Observer — extract observations from unprocessed messages
    try:
        from core.memory.observer import Observer
        from sqlalchemy import text
        observer = Observer(db, llm_client=llm_client)

        session_ids = [session_id] if session_id else [
            r[0] for r in db.execute(text(
                "SELECT session_id FROM conversation_events "
                "WHERE user_id = :uid GROUP BY session_id "
                "ORDER BY MAX(created_at) DESC LIMIT 10"
            ), {"uid": user_id}).fetchall()
        ]
        for sid in session_ids:
            # Fetch messages for this session
            rows = db.execute(text(
                "SELECT event_type, content FROM conversation_events "
                "WHERE session_id = :sid ORDER BY created_at"
            ), {"sid": sid}).fetchall()
            messages = [
                {
                    "role": "assistant" if "llm" in (r[0] or "") else "user",
                    "content": r[1],
                }
                for r in rows if r[1]
            ]
            if messages:
                kwargs = {"session_id": sid, "user_id": user_id, "messages": messages}
                if observe_threshold is not None:
                    kwargs["threshold"] = observe_threshold
                obs = observer.observe(**kwargs)
                result.observations_extracted += len(obs)
    except Exception as e:
        logger.error("Memory pipeline observer failed: %s", e)
        result.errors.append(f"observer: {e}")

    # Phase 2: Reflector — condense accumulated observations
    try:
        from core.memory.reflector import Reflector
        reflector = Reflector(db, llm_client=llm_client)
        condensed = reflector.reflect(user_id=user_id)
        result.reflections_condensed = condensed.get("after", 0)
    except Exception as e:
        logger.error("Memory pipeline reflector failed: %s", e)
        result.errors.append(f"reflector: {e}")

    # Phase 3: PollutionDetector — scan for contradictions
    quarantined_entries: list[dict] = []
    try:
        from core.context.pollution import PollutionDetector
        detector = PollutionDetector(db)
        candidates = detector.detect_pollution_candidates(user_id=user_id)
        result.contradictions_found = len(candidates)
        for c in candidates:
            if c.get("severity") in ("high", "critical"):
                detector.quarantine_entry(c["entry_id"], severity=c["severity"])
                result.quarantined += 1
                quarantined_entries.append(c)
    except Exception as e:
        logger.error("Memory pipeline pollution failed: %s", e)
        result.errors.append(f"pollution: {e}")

    # Phase 4: Knowledge regression — trace impact of quarantined entries
    if quarantined_entries:
        try:
            import os
            from core.data_versioning.knowledge_regression import KnowledgeRegression
            source_db = os.environ.get("MATRIXONE_DATABASE")
            if not source_db:
                raise RuntimeError("MATRIXONE_DATABASE env var not set; cannot run regression detection")
            kr = KnowledgeRegression(db, source_db=source_db)
            for entry in quarantined_entries:
                signal = kr.detect_knowledge_change_impact(
                    entry_id=entry["entry_id"],
                    category=entry.get("category", "unknown"),
                )
                if signal.affected_sessions > 0:
                    result.regression_signals += 1
                    logger.warning(
                        "Quarantined entry %s impacts %d sessions",
                        entry["entry_id"], signal.affected_sessions,
                    )
        except Exception as e:
            logger.error("Memory pipeline regression detection failed: %s", e)
            result.errors.append(f"regression: {e}")

    return result
