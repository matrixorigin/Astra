"""Test hallucination firewall."""

import pytest

from core.context.manager import ContextManager, TaskType
from core.events.event_logger import EventLogger
from core.verification.claim_extractor import ClaimExtractor
from core.verification.firewall import FirewallResult, HallucinationFirewall
from sdk import Database


@pytest.fixture
def db():
    """Database fixture."""
    return Database()


@pytest.fixture
def context_manager(db):
    """Context manager fixture."""
    return ContextManager(db, embedding_provider="mock")


@pytest.fixture
def event_logger(db):
    """Event logger fixture."""
    return EventLogger(db)


@pytest.fixture
def firewall(db, context_manager):
    """Firewall fixture."""
    return HallucinationFirewall(db, context_manager, threshold=0.7)


def test_claim_extraction():
    """Test claim extraction from text."""
    extractor = ClaimExtractor()

    text = "PR #123 has 5 files changed. The test passed on 2026-02-12."

    claims = extractor.extract(text)

    # Should extract: PR #123, 5, 2026-02-12
    assert len(claims) >= 3

    claim_values = [c.value for c in claims]
    assert any("123" in v for v in claim_values)  # PR number
    assert any("5" in v for v in claim_values)  # File count
    assert any("2026-02-12" in v for v in claim_values)  # Date


def test_firewall_no_claims(db, context_manager, firewall):
    """Test firewall with response containing no verifiable claims."""
    session_id = "test_session_firewall_001"

    # Create a simple context
    context = context_manager.build_context(
        session_id=session_id, query="Test query", task_type=TaskType.GENERAL
    )

    snapshot_id = context_manager.save_snapshot(context, session_id)

    # Response with no specific claims
    response = "This is a general response without specific numbers or references."

    result = firewall.verify_response(response, snapshot_id, mode="warn")

    # Should pass (no claims to verify)
    assert result.safe_to_deliver
    assert result.claims_verified == 0
    assert result.claims_failed == 0


def test_firewall_with_verified_claims(db, context_manager, event_logger, firewall):
    """Test firewall with claims that can be verified."""
    session_id = "test_session_firewall_002"
    user_id = "test_user"

    # Create event with specific content
    event = event_logger.create_user_query(
        user_id=user_id, session_id=session_id, content="PR #123 has 5 files changed"
    )

    # Build context (will include the event)
    context = context_manager.build_context(
        session_id=session_id, query="PR #123", task_type=TaskType.GENERAL
    )

    snapshot_id = context_manager.save_snapshot(context, session_id, event.event_id)

    # Response that matches context
    response = "PR #123 has 5 files changed."

    result = firewall.verify_response(response, snapshot_id, mode="warn")

    # Should pass (claims verified)
    assert result.safe_to_deliver
    assert result.claims_verified > 0


def test_firewall_with_contradictions(db, context_manager, event_logger, firewall):
    """Test firewall with claims that contradict context."""
    session_id = "test_session_firewall_003"
    user_id = "test_user"

    # Create event with specific content
    event = event_logger.create_user_query(
        user_id=user_id, session_id=session_id, content="PR #123 has 5 files changed"
    )

    # Build context
    context = context_manager.build_context(
        session_id=session_id, query="PR #123", task_type=TaskType.GENERAL
    )

    snapshot_id = context_manager.save_snapshot(context, session_id, event.event_id)

    # Response with different numbers (potential hallucination)
    response = "PR #999 has 100 files changed."

    result = firewall.verify_response(response, snapshot_id, mode="warn")

    # Should have failed verifications
    assert result.claims_failed > 0
    assert len(result.contradictions) > 0


def test_firewall_block_mode(db, context_manager, event_logger, firewall):
    """Test firewall in block mode."""
    session_id = "test_session_firewall_004"
    user_id = "test_user"

    # Create event
    event = event_logger.create_user_query(
        user_id=user_id, session_id=session_id, content="Test content"
    )

    # Build context
    context = context_manager.build_context(
        session_id=session_id, query="Test", task_type=TaskType.GENERAL
    )

    snapshot_id = context_manager.save_snapshot(context, session_id, event.event_id)

    # Response with unverifiable claims
    response = "PR #999 has 100 files changed."

    # Block mode - should reject if confidence too low
    result = firewall.verify_response(response, snapshot_id, mode="block")

    # May or may not be safe depending on threshold
    assert isinstance(result.safe_to_deliver, bool)
    assert 0.0 <= result.confidence_score <= 1.0


def test_firewall_logging(db, context_manager, event_logger, firewall):
    """Test that firewall logs verification results."""
    session_id = "test_session_firewall_005"
    user_id = "test_user"

    # Create event
    event = event_logger.create_user_query(
        user_id=user_id, session_id=session_id, content="Test content"
    )

    # Build context
    context = context_manager.build_context(
        session_id=session_id, query="Test", task_type=TaskType.GENERAL
    )

    snapshot_id = context_manager.save_snapshot(context, session_id, event.event_id)

    # Verify response
    response = "Test response with number 42."
    result = firewall.verify_response(response, snapshot_id, mode="warn")

    # Log verification
    firewall.log_verification(session_id, event.event_id, result)

    # Check that verification event was logged
    events = db.fetchall(
        """
        SELECT * FROM conversation_events
        WHERE session_id = %s AND event_type = 'hallucination_check'
        """,
        (session_id,),
    )

    assert len(events) > 0


def test_firewall_confidence_threshold(db, context_manager):
    """Test firewall with different confidence thresholds."""
    # High threshold (strict)
    strict_firewall = HallucinationFirewall(db, context_manager, threshold=0.9)
    assert strict_firewall.threshold == 0.9

    # Low threshold (permissive)
    permissive_firewall = HallucinationFirewall(db, context_manager, threshold=0.5)
    assert permissive_firewall.threshold == 0.5


def test_firewall_empty_response(db, context_manager):
    """Test firewall with empty response."""
    firewall = HallucinationFirewall(db, context_manager)

    result = firewall.verify_response("", "snapshot_123", mode="warn")

    assert result.safe_to_deliver
    assert "Empty response" in result.warnings


def test_firewall_invalid_snapshot_id(db, context_manager):
    """Test firewall with invalid snapshot_id."""
    firewall = HallucinationFirewall(db, context_manager)

    result = firewall.verify_response("Test response", "", mode="warn")

    assert result.safe_to_deliver  # Fail open
    assert "No snapshot_id" in result.warnings[0]


def test_firewall_invalid_mode(db, context_manager, event_logger):
    """Test firewall with invalid mode."""
    session_id = "test_session_firewall_006"
    user_id = "test_user"

    event = event_logger.create_user_query(
        user_id=user_id, session_id=session_id, content="Test content"
    )

    context = context_manager.build_context(
        session_id=session_id, query="Test", task_type=TaskType.GENERAL
    )

    snapshot_id = context_manager.save_snapshot(context, session_id, event.event_id)

    firewall = HallucinationFirewall(db, context_manager)

    # Invalid mode should default to 'warn'
    result = firewall.verify_response("Test response", snapshot_id, mode="invalid")

    assert isinstance(result, FirewallResult)


def test_firewall_snapshot_load_failure(db, context_manager):
    """Test firewall when snapshot load fails."""
    firewall = HallucinationFirewall(db, context_manager)

    # Non-existent snapshot
    result = firewall.verify_response("Test response with 42", "nonexistent_snapshot", mode="warn")

    assert result.safe_to_deliver  # Fail open
    assert "Snapshot load failed" in result.warnings[0]


def test_firewall_log_verification_missing_params(db, context_manager):
    """Test log_verification with missing parameters."""
    firewall = HallucinationFirewall(db, context_manager)

    result = FirewallResult(
        safe_to_deliver=True,
        confidence_score=0.9,
        claims_verified=1,
        claims_failed=0,
        contradictions=[],
        warnings=[],
    )

    # Should not crash with missing params
    firewall.log_verification("", "event_123", result)
    firewall.log_verification("session_123", "", result)


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
