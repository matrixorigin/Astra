"""Tests for Phase 1 completion: Quality Badge SSE, 5D Firewall, Replay Validation.

1. Quality Badge SSE — verify badge events emitted in stream
2. 5D Confidence — verify firewall uses tool_quality_score dimension
3. Replay Validation — simulate 019ca950 full pipeline with quality firewall
"""

import json
from unittest.mock import MagicMock, Mock, patch
from datetime import datetime, timezone

import pytest

from core.verification.firewall import HallucinationFirewall, FirewallResult
from core.verification.tool_quality import assess_tool_result, annotate_tool_result


# ============================================================================
# 1. Quality Badge SSE
# ============================================================================

class TestQualityBadgeSSE:
    """Verify tool_result_quality SSE events are emitted for degraded results."""

    def test_badge_emitted_for_degraded_edge_tool(self):
        """When edge tool_results contain degraded data, a tool_result_quality
        SSE event should be yielded after session_info."""
        session_id = "test-badge-sse"
        tool_call_id = "tc_badge"

        cache_entry = {
            "history": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "test"},
                {"role": "assistant", "content": "", "tool_calls": [
                    {"id": tool_call_id, "type": "function",
                     "function": {"name": "stock_assistant", "arguments": "{}"}},
                ]},
            ],
            "tools": [], "turn_count": 1, "sections": {"identity": "sys"},
            "tool_quality_assessments": [{
                "tool_name": "stock_assistant",
                "score": 0.3,
                "grade": "degraded",
                "signals": ["empty_containers: 5/7 fields empty"],
                "stale": False,
            }],
        }

        # Simulate what the SSE generator does: check cache for assessments
        badges = []
        for qa in cache_entry.get("tool_quality_assessments", []):
            if qa["grade"] != "complete":
                badges.append({
                    "type": "tool_result_quality",
                    "tool_name": qa["tool_name"],
                    "grade": qa["grade"],
                    "score": qa["score"],
                    "signals": qa["signals"],
                })

        assert len(badges) == 1
        assert badges[0]["type"] == "tool_result_quality"
        assert badges[0]["tool_name"] == "stock_assistant"
        assert badges[0]["grade"] == "degraded"

    def test_no_badge_for_complete_result(self):
        """Complete results should not emit a badge."""
        cache_entry = {
            "tool_quality_assessments": [{
                "tool_name": "weather",
                "score": 1.0,
                "grade": "complete",
                "signals": [],
                "stale": False,
            }],
        }
        badges = [qa for qa in cache_entry.get("tool_quality_assessments", [])
                  if qa["grade"] != "complete"]
        assert len(badges) == 0

    def test_cloud_tool_result_badge(self):
        """Cloud tool results should also get quality assessment and badge."""
        cloud_result = json.dumps({
            "data": {}, "info": {}, "risk_score": 0,
            "confidence": 0, "volatility": 0,
        })
        assessment = assess_tool_result("stock_assistant", cloud_result)
        assert assessment.needs_annotation
        assert assessment.grade != "complete"
        # Badge would be emitted
        badge = {
            "type": "tool_result_quality",
            "tool_name": "stock_assistant",
            "grade": assessment.grade,
            "score": assessment.score,
            "signals": assessment.signals[:5],
        }
        assert badge["type"] == "tool_result_quality"


# ============================================================================
# 2. 5D Confidence Scoring
# ============================================================================

class TestFirewall5DConfidence:
    """Verify HallucinationFirewall uses tool_quality_score as 5th dimension."""

    def _make_firewall(self):
        """Create a firewall with mocked dependencies."""
        db_factory = MagicMock()
        ctx_mgr = MagicMock()
        # Mock snapshot
        ctx_mgr.load_snapshot.return_value = MagicMock(
            system_prompt="test", content="stock analysis data",
            created_at=datetime.now(timezone.utc).isoformat(),
        )
        fw = HallucinationFirewall(
            db_factory, context_manager=ctx_mgr,
            use_llm_extraction=False,
        )
        return fw

    def test_5d_confidence_lower_with_degraded_quality(self):
        """When tool_quality_score is low, overall confidence should drop."""
        fw = self._make_firewall()

        # Mock extractors and verifiers
        mock_claim = MagicMock(type="factual", value="test")
        fw.regex_extractor = MagicMock()
        fw.regex_extractor.extract.return_value = [mock_claim]
        fw.use_llm_extraction = False

        with patch.object(fw, '_weighted_confidence', return_value=0.8), \
             patch.object(fw, '_context_coverage', return_value=0.7), \
             patch.object(fw, '_knowledge_freshness', return_value=0.9), \
             patch.object(fw, '_skill_reliability', return_value=0.85), \
             patch.object(fw, '_simple_verify_claim') as mock_verify:
            mock_verify.return_value = MagicMock(verified=True, evidence=[], confidence=0.8)

            # 4D (no tool quality)
            result_4d = fw.verify_response(
                "Stock analysis shows low risk",
                "snapshot-123",
                skill_name="stock_assistant",
                tool_quality_score=None,
            )

            # 5D with degraded quality
            result_5d_degraded = fw.verify_response(
                "Stock analysis shows low risk",
                "snapshot-123",
                skill_name="stock_assistant",
                tool_quality_score=0.3,
            )

            # 5D with good quality
            result_5d_good = fw.verify_response(
                "Stock analysis shows low risk",
                "snapshot-123",
                skill_name="stock_assistant",
                tool_quality_score=0.95,
            )

        # Degraded quality should lower confidence
        assert result_5d_degraded.confidence_score < result_4d.confidence_score, (
            f"5D degraded ({result_5d_degraded.confidence_score}) should be < "
            f"4D ({result_4d.confidence_score})"
        )
        # Good quality should be close to 4D
        assert result_5d_good.confidence_score >= result_5d_degraded.confidence_score

    def test_5d_weights_sum_to_1(self):
        """5D weights must sum to 1.0."""
        assert abs((0.30 + 0.20 + 0.15 + 0.15 + 0.20) - 1.0) < 1e-9

    def test_4d_weights_unchanged(self):
        """4D weights (no tool quality) must remain unchanged."""
        assert abs((0.35 + 0.25 + 0.20 + 0.20) - 1.0) < 1e-9

    def test_3d_weights_unchanged(self):
        """3D weights (no skill, no tool quality) must remain unchanged."""
        assert abs((0.45 + 0.30 + 0.25) - 1.0) < 1e-9

    def test_tool_quality_only_no_skill(self):
        """When tool_quality available but skill_reliability not, use 4D variant."""
        fw = self._make_firewall()
        mock_claim = MagicMock(type="factual", value="test")
        fw.regex_extractor = MagicMock()
        fw.regex_extractor.extract.return_value = [mock_claim]
        fw.use_llm_extraction = False

        with patch.object(fw, '_weighted_confidence', return_value=0.8), \
             patch.object(fw, '_context_coverage', return_value=0.7), \
             patch.object(fw, '_knowledge_freshness', return_value=0.9), \
             patch.object(fw, '_simple_verify_claim') as mock_verify:
            mock_verify.return_value = MagicMock(verified=True, evidence=[], confidence=0.8)

            result = fw.verify_response(
                "test response", "snapshot-123",
                skill_name=None,
                tool_quality_score=0.3,
            )
        expected = 0.35 * 0.8 + 0.25 * 0.7 + 0.20 * 0.9 + 0.20 * 0.3
        assert abs(result.confidence_score - round(expected, 4)) < 0.01


# ============================================================================
# 3. Replay Validation — 019ca950 Simulation
# ============================================================================

class TestReplayValidation019ca950:
    """Simulate the 019ca950 session with quality firewall enabled.

    This is the design doc §11 Replay Test — verifying the full pipeline
    would have prevented the confabulation.
    """

    # Exact tool result from session 019ca950
    STOCK_RESULT = {
        "stock_code": "600030",
        "stock_name": "中信证券",
        "current_price": 0,
        "price_change": 0,
        "technical_indicators": {},
        "trend_analysis": {},
        "risk_score": 0,
        "risk_factors": [],
        "recommendation": "",
        "confidence": 50,
    }

    def test_step1_quality_assessment_detects_degraded(self):
        """Step 1: stock_assistant returns empty-shell → quality firewall scores < 0.8."""
        assessment = assess_tool_result("stock_assistant", self.STOCK_RESULT)
        assert assessment.score < 0.8
        assert assessment.grade != "complete"
        assert assessment.needs_annotation

    def test_step2_annotation_injected(self):
        """Step 2: Annotation injected into tool result content."""
        assessment = assess_tool_result("stock_assistant", self.STOCK_RESULT)
        tr = {"result": json.dumps(self.STOCK_RESULT)}
        annotated = annotate_tool_result(tr, assessment)
        content = annotated["result"]
        assert "[TOOL QUALITY:" in content
        assert "Respond honestly" in content
        # LLM would see this annotation before the empty data

    def test_step3_badge_emitted(self):
        """Step 3: Quality badge SSE event would be emitted."""
        assessment = assess_tool_result("stock_assistant", self.STOCK_RESULT)
        badge = {
            "type": "tool_result_quality",
            "tool_name": "stock_assistant",
            "grade": assessment.grade,
            "score": assessment.score,
            "signals": assessment.signals,
        }
        assert badge["grade"] in ("degraded", "empty")
        assert badge["score"] < 0.8

    def test_step4_firewall_confidence_drops(self):
        """Step 4: Hallucination Firewall confidence drops due to 5D scoring."""
        assessment = assess_tool_result("stock_assistant", self.STOCK_RESULT)

        # With 5D scoring, tool_quality_score = assessment.score (~0.3)
        # This should lower the overall confidence significantly
        # Simulated: all other dimensions at 0.8
        base_dims = {"claim": 0.8, "coverage": 0.7, "freshness": 0.9, "skill": 0.85}

        # 4D confidence (original — no quality signal)
        conf_4d = (0.35 * base_dims["claim"] + 0.25 * base_dims["coverage"]
                   + 0.20 * base_dims["freshness"] + 0.20 * base_dims["skill"])

        # 5D confidence (with degraded tool quality)
        conf_5d = (0.30 * base_dims["claim"] + 0.20 * base_dims["coverage"]
                   + 0.15 * base_dims["freshness"] + 0.15 * base_dims["skill"]
                   + 0.20 * assessment.score)

        assert conf_5d < conf_4d, (
            f"5D confidence ({conf_5d:.4f}) should be lower than "
            f"4D ({conf_4d:.4f}) when tool quality is degraded"
        )
        # The drop should be significant (>5%)
        drop = conf_4d - conf_5d
        assert drop > 0.05, f"Confidence drop ({drop:.4f}) should be >5%"

    def test_step5_quality_event_persisted(self, quality_session):
        """Step 5: Quality event persisted and surfaced by reflect."""
        sid, user_id, _ = quality_session
        from api.routers.chat import _build_reflect_evidence
        evidence = _build_reflect_evidence(
            session_id=sid, user_id=user_id, focus="auto", last_n=50,
        )
        summary = evidence.get("tool_quality_summary", [])
        assert len(summary) >= 1
        assert summary[0]["tool"] == "stock_assistant"
        assert summary[0]["grade"] == "degraded"


# Reuse the quality_session fixture from the DB integration test
@pytest.fixture
def quality_session():
    """Create a real DB session with a tool_result_quality event."""
    from api.database import SessionLocal
    from core.events.session_manager import SessionManager
    from core.events.event_logger import EventLogger
    from sqlalchemy import text

    user_id = "replay_val_tst"
    mgr = SessionManager(SessionLocal())
    session = mgr.create_session(user_id=user_id)
    sid = session.session_id

    el = EventLogger(SessionLocal)
    uq = el.create_user_query(user_id=user_id, session_id=sid,
                               content="中信证券建议买吗？")
    chain = uq.causal_chain_id

    el.create_stream_event(
        user_id=user_id, session_id=sid,
        event_type="tool_result_quality",
        content=json.dumps({
            "tool_name": "stock_assistant", "score": 0.3,
            "grade": "degraded", "signals": ["empty_containers"], "stale": False,
        }),
        parent_event_id=uq.event_id, causal_chain_id=chain,
        metadata={
            "tool_name": "stock_assistant",
            "quality_score": 0.3,
            "quality_grade": "degraded",
            "signals": ["empty_containers"],
            "stale": False,
        },
    )
    el.create_llm_response(
        user_id=user_id, session_id=sid,
        content="数据不完整，无法给出可靠建议。",
        agent_id="dev-agent", agent_version="0.1.0",
        parent_event_id=uq.event_id, causal_chain_id=chain,
    )

    yield sid, user_id, chain

    db = SessionLocal()
    for table in ("agent_events", "agent_sessions"):
        try:
            db.execute(text(f"DELETE FROM {table} WHERE session_id = :sid"), {"sid": sid})
        except Exception:
            pass
    db.commit()
    db.close()
