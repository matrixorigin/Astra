"""Integration tests for Phase 2/3/4: Schema DB storage, learning signal persistence.

Verifies REAL database operations:
  1. quality_schema column in skills_registry — write and read via schema loader
  2. LOW_DATA_QUALITY signal persisted to skill_learnings table
  3. End-to-end: skill with schema → assess_tool_result → correct grade/score
"""

import json
import pytest
from datetime import datetime, timezone

from sqlalchemy import text

from api.database import SessionLocal
from core.skills.learning_signals import SignalType, SignalThresholds
from core.verification.tool_quality import (
    assess_tool_result,
    set_schema_loader,
    invalidate_schema_cache,
)


# ============================================================================
# Fixtures
# ============================================================================

@pytest.fixture
def skill_with_schema(db_session):
    """Create a skill with quality_schema in skills_registry."""
    db = SessionLocal()
    skill_name = f"test_schema_skill_{datetime.now().timestamp():.0f}"
    skill_id = f"{skill_name}@1.0.0"
    schema = {
        "required_fields": [
            {"path": "data.value", "type": "number"},
            {"path": "data.items", "type": "list", "min_length": 1},
        ],
        "sentinel_values": [
            {"path": "data.status", "sentinel": "unknown", "meaning": "not fetched"},
        ],
    }
    try:
        # Ensure quality_schema column exists (migration may not have run)
        try:
            db.execute(text("ALTER TABLE skills_registry ADD COLUMN quality_schema JSON"))
            db.commit()
        except Exception:
            db.rollback()  # Column already exists
        
        db.execute(
            text("""
                INSERT INTO skills_registry 
                (skill_id, skill_name, version, description, quality_schema, is_active, created_at)
                VALUES (:id, :name, '1.0.0', 'Test skill', :schema, 1, NOW())
            """),
            {"id": skill_id, "name": skill_name, "schema": json.dumps(schema)},
        )
        db.commit()
        yield skill_name, schema
    finally:
        db.execute(
            text("DELETE FROM skills_registry WHERE skill_name = :name"),
            {"name": skill_name},
        )
        db.commit()
        db.close()


@pytest.fixture
def schema_loader_from_db():
    """Set up schema loader that reads from real DB."""
    def _load(tool_name: str) -> dict | None:
        db = SessionLocal()
        try:
            row = db.execute(
                text("""
                    SELECT quality_schema FROM skills_registry 
                    WHERE skill_name = :name AND is_active = 1 
                    ORDER BY created_at DESC LIMIT 1
                """),
                {"name": tool_name},
            ).first()
            if row and row[0]:
                return row[0] if isinstance(row[0], dict) else json.loads(row[0])
        finally:
            db.close()
        return None

    invalidate_schema_cache()
    set_schema_loader(_load)
    yield
    set_schema_loader(None)
    invalidate_schema_cache()


# ============================================================================
# Phase 2: Schema DB Storage
# ============================================================================

class TestSchemaDBStorage:
    """Verify quality_schema column works end-to-end."""

    def test_schema_stored_and_retrieved(self, skill_with_schema, schema_loader_from_db):
        """Schema written to DB can be loaded by schema loader."""
        skill_name, expected_schema = skill_with_schema
        
        from core.verification.tool_quality import load_quality_schema
        loaded = load_quality_schema(skill_name)
        
        assert loaded is not None, f"Schema not found for {skill_name}"
        assert loaded["required_fields"] == expected_schema["required_fields"]
        assert loaded["sentinel_values"] == expected_schema["sentinel_values"]

    def test_assess_uses_db_schema(self, skill_with_schema, schema_loader_from_db):
        """assess_tool_result uses schema from DB for Tier 1 assessment."""
        skill_name, _ = skill_with_schema
        
        # Empty result should fail schema validation
        result = {"data": {"value": None, "items": [], "status": "unknown"}}
        assessment = assess_tool_result(skill_name, result)
        
        # Verify schema-based assessment detected issues
        assert assessment.grade != "complete"
        assert assessment.score < 1.0
        # Should detect: missing value, empty items, sentinel status
        assert len(assessment.signals) >= 2

    def test_complete_result_passes_db_schema(self, skill_with_schema, schema_loader_from_db):
        """Complete result should pass schema validation."""
        skill_name, _ = skill_with_schema
        
        result = {"data": {"value": 42, "items": ["a", "b"], "status": "ok"}}
        assessment = assess_tool_result(skill_name, result)
        
        assert assessment.grade == "complete"
        assert assessment.score >= 0.8

    def test_schema_column_json_type(self, skill_with_schema):
        """Verify quality_schema column stores JSON correctly."""
        skill_name, expected_schema = skill_with_schema
        db = SessionLocal()
        try:
            row = db.execute(
                text("SELECT quality_schema FROM skills_registry WHERE skill_name = :name"),
                {"name": skill_name},
            ).first()
            
            assert row is not None
            stored = row[0]
            # Should be dict (JSON column) or string (TEXT column)
            if isinstance(stored, str):
                stored = json.loads(stored)
            
            assert stored["required_fields"][0]["path"] == "data.value"
            assert stored["sentinel_values"][0]["sentinel"] == "unknown"
        finally:
            db.close()


# ============================================================================
# Phase 3: Learning Signal Persistence
# ============================================================================

class TestLearningSignalPersistence:
    """Verify LOW_DATA_QUALITY signal is persisted to skill_learnings."""

    def test_low_quality_signal_persisted(self, db_session):
        """SelfImprovingSelector persists LOW_DATA_QUALITY to DB."""
        from core.skills.self_improving_selector import SelfImprovingSelector
        
        db = SessionLocal()
        query_pattern = f"test_quality_signal_{datetime.now().timestamp():.0f}"
        
        try:
            # Create selector with real DB
            selector = SelfImprovingSelector(
                db_factory=lambda: SessionLocal(),
                thresholds=SignalThresholds(low_data_quality=0.5),
            )
            
            # Process a low quality failure
            failure = {
                "user_query": query_pattern,
                "selected_skills": ["bad_tool"],
                "tool_quality_score": 0.2,  # Below threshold
            }
            
            # Extract and persist signal
            signal = selector._extract_signal(failure, SignalType.LOW_DATA_QUALITY)
            assert signal is not None
            
            selector._update_learnings(signal)
            
            # Verify persisted to DB
            row = db.execute(
                text("""
                    SELECT signal_type, query_pattern, target_metrics, confidence
                    FROM skill_selection_learnings 
                    WHERE query_pattern = :pattern AND signal_type = :sig
                """),
                {"pattern": query_pattern, "sig": "low_data_quality"},
            ).first()
            
            assert row is not None, "Learning signal not found in DB"
            assert row[0] == "low_data_quality"
            assert row[1] == query_pattern
            
            # Check target_metrics
            metrics = row[2] if isinstance(row[2], dict) else json.loads(row[2])
            assert "quality_score" in metrics
            assert metrics["quality_score"] == 0.8  # threshold + 0.3
            
            # Check confidence
            assert row[3] >= 10  # Initial confidence
            
        finally:
            # Cleanup
            db.execute(
                text("DELETE FROM skill_selection_learnings WHERE query_pattern = :pattern"),
                {"pattern": query_pattern},
            )
            db.commit()
            db.close()

    def test_repeated_signals_increase_confidence(self, db_session):
        """Multiple LOW_DATA_QUALITY signals increase evidence_count and confidence."""
        from core.skills.self_improving_selector import SelfImprovingSelector
        
        db = SessionLocal()
        query_pattern = f"test_confidence_{datetime.now().timestamp():.0f}"
        
        try:
            selector = SelfImprovingSelector(
                db_factory=lambda: SessionLocal(),
                thresholds=SignalThresholds(low_data_quality=0.5),
            )
            
            failure = {
                "user_query": query_pattern,
                "selected_skills": ["flaky_tool"],
                "tool_quality_score": 0.3,
            }
            
            # First signal
            signal1 = selector._extract_signal(failure, SignalType.LOW_DATA_QUALITY)
            selector._update_learnings(signal1)
            
            row1 = db.execute(
                text("SELECT evidence_count, confidence FROM skill_selection_learnings WHERE query_pattern = :p"),
                {"p": query_pattern},
            ).first()
            count1, conf1 = row1
            
            # Second signal (same pattern)
            signal2 = selector._extract_signal(failure, SignalType.LOW_DATA_QUALITY)
            selector._update_learnings(signal2)
            
            row2 = db.execute(
                text("SELECT evidence_count, confidence FROM skill_selection_learnings WHERE query_pattern = :p"),
                {"p": query_pattern},
            ).first()
            count2, conf2 = row2
            
            assert count2 > count1, "evidence_count should increase"
            assert conf2 > conf1, "confidence should increase"
            
        finally:
            db.execute(
                text("DELETE FROM skill_selection_learnings WHERE query_pattern = :pattern"),
                {"pattern": query_pattern},
            )
            db.commit()
            db.close()


# ============================================================================
# End-to-End: Full Pipeline
# ============================================================================

class TestE2EQualityPipeline:
    """End-to-end test: schema → assessment → auto-score."""

    def test_full_pipeline(self, skill_with_schema, schema_loader_from_db):
        """Complete flow: DB schema → assess → auto-score with quality factor."""
        from core.evaluation.auto_scorer import compute_auto_score
        from core.verification.tool_quality import response_acknowledges_limitation
        
        skill_name, _ = skill_with_schema
        
        # 1. Degraded tool result
        degraded_result = {"data": {"value": None, "items": [], "status": "unknown"}}
        assessment = assess_tool_result(skill_name, degraded_result)
        
        assert assessment.score < 0.5
        assert assessment.grade in ("degraded", "empty")
        
        # 2. LLM response that acknowledges limitation
        response_ack = "数据不完整，无法给出可靠建议。"
        assert response_acknowledges_limitation(response_ack)
        
        # 3. Auto-score with quality data
        score_with_ack = compute_auto_score(
            firewall_passed=True,
            firewall_confidence=0.8,
            response_tokens=100,
            tool_quality_score=assessment.score,
            data_quality_acknowledged=True,
        )
        
        # 4. Compare with non-acknowledged
        score_no_ack = compute_auto_score(
            firewall_passed=True,
            firewall_confidence=0.8,
            response_tokens=100,
            tool_quality_score=assessment.score,
            data_quality_acknowledged=False,
        )
        
        # Acknowledged should get bonus
        assert score_with_ack.quality_score > score_no_ack.quality_score
        
        # 5. Compare with no quality data (backward compat)
        score_no_quality = compute_auto_score(
            firewall_passed=True,
            firewall_confidence=0.8,
            response_tokens=100,
        )
        
        # Without quality data, uses original formula
        assert score_no_quality.quality_score > score_no_ack.quality_score
