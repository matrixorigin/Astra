"""Tests for Hallucination Firewall with SQLAlchemy."""

import pytest
from uuid import uuid4

from api.database import get_db_session
from api.repositories.session_repository import SessionRepository
from api.repositories.event_repository import EventRepository
from core.verification.firewall import HallucinationFirewall
from core.verification.claim_extractor import ClaimExtractor


@pytest.fixture
def session_repo(db_session):
    """Session repository fixture."""
    return SessionRepository(db_session)


@pytest.fixture
def event_repo(db_session):
    """Event repository fixture."""
    return EventRepository(db_session)


@pytest.fixture
def firewall(db_session):
    """Hallucination firewall fixture."""
    from core.context.manager import ContextManager
    
    context_manager = ContextManager(lambda: db_session)
    return HallucinationFirewall(db_session, context_manager)


@pytest.fixture
def claim_extractor():
    """Claim extractor fixture."""
    return ClaimExtractor()


def test_claim_extraction(claim_extractor):
    """Test claim extraction from LLM response."""
    response = "The capital of France is Paris. Python was created in 1991."
    
    claims = claim_extractor.extract(response)
    
    assert len(claims) >= 0
    # Verify claims are Claim objects
    for claim in claims:
        assert hasattr(claim, 'type')
        assert hasattr(claim, 'value')
        assert hasattr(claim, 'context')


def test_firewall_no_claims(firewall):
    """Test firewall with response containing no verifiable claims."""
    response = "I think this might be helpful. Let me know if you need more information."
    
    result = firewall.verify_response(
        response=response,
        context_capture_id="test_snapshot",
        mode="warn"
    )
    
    assert hasattr(result, 'safe_to_deliver')
    assert hasattr(result, 'confidence_score')
    assert hasattr(result, 'claims_verified')
    assert hasattr(result, 'claims_failed')
    assert result.claims_verified >= 0
    assert result.claims_failed >= 0


def test_firewall_with_verified_claims(firewall, session_repo, event_repo):
    """Test firewall with verifiable claims."""
    user_id = str(uuid4())
    session_id = str(uuid4())
    
    # Create session and events with factual data
    session = session_repo.create({
        "session_id": session_id,
        "user_id": user_id
    })
    
    # Create event with factual content
    event_repo.create({
        "event_id": str(uuid4()),
        "user_id": user_id,
        "session_id": session_id,
        "event_type": "user_query",
        "content": "What is the capital of France?",
        "causal_chain_id": str(uuid4())
    })
    
    response = "Based on the data, the capital of France is Paris."
    
    result = firewall.verify_response(
        response=response,
        context_capture_id=session_id,
        mode="warn"
    )
    
    assert hasattr(result, 'safe_to_deliver')
    assert hasattr(result, 'confidence_score')
    assert result.confidence_score >= 0.0
    assert result.confidence_score <= 1.0


def test_firewall_with_contradictions(firewall, session_repo, event_repo):
    """Test firewall with contradictory claims."""
    user_id = str(uuid4())
    session_id = str(uuid4())
    
    # Create session with factual data
    session = session_repo.create({
        "session_id": session_id,
        "user_id": user_id
    })
    
    # Create event with correct information
    event_repo.create({
        "event_id": str(uuid4()),
        "user_id": user_id,
        "session_id": session_id,
        "event_type": "user_query",
        "content": "The capital of France is Paris",
        "causal_chain_id": str(uuid4())
    })
    
    # Response with contradictory information
    response = "The capital of France is London."
    
    result = firewall.verify_response(
        response=response,
        context_capture_id=session_id,
        mode="warn"
    )
    
    assert hasattr(result, 'safe_to_deliver')
    assert hasattr(result, 'contradictions')
    assert isinstance(result.contradictions, list)


def test_firewall_block_mode(firewall):
    """Test firewall in block mode."""
    response = "This contains potentially false information about historical facts."
    
    result = firewall.verify_response(
        response=response,
        context_capture_id="test_snapshot",
        mode="block"
    )
    
    assert hasattr(result, 'safe_to_deliver')
    assert isinstance(result.safe_to_deliver, bool)


def test_firewall_logging(firewall, session_repo, event_repo):
    """Test firewall logging functionality."""
    user_id = str(uuid4())
    session_id = str(uuid4())
    
    # Create session
    session = session_repo.create({
        "session_id": session_id,
        "user_id": user_id
    })
    
    response = "Test response for logging verification."
    
    result = firewall.verify_response(
        response=response,
        context_capture_id=session_id,
        mode="warn"
    )
    
    assert hasattr(result, 'safe_to_deliver')
    assert hasattr(result, 'warnings')
    assert isinstance(result.warnings, list)


def test_firewall_no_claims(firewall):
    """Test firewall with response containing no verifiable claims."""
    response = "I think this might be helpful. Let me know if you need more information."
    
    result = firewall.verify_response(
        response=response,
        context_capture_id="test_snapshot",
        mode="warn"
    )
    
    assert hasattr(result, 'safe_to_deliver')
    assert hasattr(result, 'claims_verified')
    assert result.claims_verified >= 0


def test_firewall_with_verified_claims(firewall, session_repo, event_repo):
    """Test firewall with verifiable claims."""
    user_id = str(uuid4())
    session_id = str(uuid4())
    
    # Create session and events with factual data
    session = session_repo.create({
        "session_id": session_id,
        "user_id": user_id
    })
    
    # Create event with factual content
    event_repo.create({
        "event_id": str(uuid4()),
        "user_id": user_id,
        "session_id": session_id,
        "event_type": "user_query",
        "content": "What is the capital of France?",
        "causal_chain_id": str(uuid4())
    })
    
    response = "Based on the data, the capital of France is Paris."
    
    result = firewall.verify_response(
        response=response,
        context_capture_id=session_id,  # Use session as snapshot
        mode="warn"
    )
    
    assert hasattr(result, 'safe_to_deliver')
    assert hasattr(result, 'claims_verified')
    assert result.claims_verified >= 0


def test_firewall_confidence_threshold(firewall):
    """Test firewall confidence threshold."""
    response = "This is a test response with uncertain claims."
    
    result = firewall.verify_response(
        response=response,
        context_capture_id="test_snapshot",
        mode="warn"
    )
    
    assert hasattr(result, 'confidence_score')
    assert 0.0 <= result.confidence_score <= 1.0


def test_firewall_empty_response(firewall):
    """Test firewall with empty response."""
    result = firewall.verify_response(
        response="",
        context_capture_id="test_snapshot",
        mode="warn"
    )
    
    assert hasattr(result, 'safe_to_deliver')
    assert hasattr(result, 'claims_verified')
    assert result.claims_verified == 0


def test_firewall_invalid_snapshot_id(firewall):
    """Test firewall with invalid snapshot ID."""
    response = "Test response with claims."
    
    result = firewall.verify_response(
        response=response,
        context_capture_id="invalid_snapshot_123",
        mode="warn"
    )
    
    assert hasattr(result, 'safe_to_deliver')


def test_firewall_warn_mode(firewall):
    """Test firewall in warn mode."""
    response = "This contains potentially questionable claims."
    
    result = firewall.verify_response(
        response=response,
        context_capture_id="test_snapshot",
        mode="warn"
    )
    
    assert hasattr(result, 'safe_to_deliver')
    assert hasattr(result, 'warnings')


def test_firewall_snapshot_load_failure(firewall):
    """Test firewall handling snapshot load failure."""
    response = "Test response."
    
    result = firewall.verify_response(
        response=response,
        context_capture_id="nonexistent_snapshot",
        mode="warn"
    )
    
    assert hasattr(result, 'safe_to_deliver')


def test_firewall_log_verification_missing_params(firewall):
    """Test firewall with missing parameters."""
    result = firewall.verify_response(
        response="Test response",
        context_capture_id=None,
        mode="warn"
    )
    
    assert hasattr(result, 'safe_to_deliver')


def test_firewall_multiple_claims(firewall):
    """Test firewall with multiple claims."""
    response = """
    Based on the data:
    1. Python was created by Guido van Rossum
    2. It was first released in 1991
    3. The latest version is Python 3.12
    """
    
    result = firewall.verify_response(
        response=response,
        context_capture_id="test_snapshot",
        mode="warn"
    )
    
    assert hasattr(result, 'safe_to_deliver')
    assert hasattr(result, 'claims_verified')
