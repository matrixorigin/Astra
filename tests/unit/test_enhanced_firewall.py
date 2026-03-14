"""Tests for enhanced hallucination firewall with LLM extraction and evidence backlinks."""

import json
from unittest.mock import MagicMock, Mock

import pytest

from core.verification.firewall import HallucinationFirewall, FirewallResult
from core.verification.llm_claim_extractor import Claim, LLMClaimExtractor
from core.verification.structured_verifier import (
    Evidence,
    StructuredVerifier,
    VerificationResult,
)


class TestLLMClaimExtractor:
    """Test LLM-based claim extraction."""

    def test_extract_claims_success(self):
        """Test successful claim extraction."""
        llm_client = Mock()
        llm_client.generate.return_value = json.dumps(
            [
                {
                    "type": "numeric",
                    "value": "5 files",
                    "context": "Changed 5 files",
                    "position": 10,
                },
                {
                    "type": "causal",
                    "value": "test fails because timeout",
                    "context": "The test fails because timeout",
                    "position": 50,
                },
            ]
        )

        extractor = LLMClaimExtractor(llm_client)
        claims = extractor.extract("Changed 5 files. The test fails because timeout.")

        assert len(claims) == 2
        assert claims[0].type == "numeric"
        assert claims[0].value == "5 files"
        assert claims[1].type == "causal"

    def test_extract_empty_text(self):
        """Test extraction from empty text."""
        llm_client = Mock()
        extractor = LLMClaimExtractor(llm_client)

        claims = extractor.extract("")
        assert claims == []

    def test_extract_json_parse_error(self):
        """Test handling of JSON parse errors."""
        llm_client = Mock()
        llm_client.generate.return_value = "invalid json"

        extractor = LLMClaimExtractor(llm_client)
        claims = extractor.extract("Some text")

        assert claims == []


class TestStructuredVerifier:
    """Test structured verification with evidence backlinks."""

    def test_verify_claim_with_event_evidence(self):
        """Test verification with evidence from events."""
        llm_client = Mock()
        llm_client.generate.return_value = json.dumps(
            {"verified": True, "confidence": 0.95, "reasoning": "Found in events"}
        )

        verifier = StructuredVerifier(llm_client)

        claim = Claim(type="numeric", value="5 files", context="Changed 5 files", position=10)

        snapshot = Mock()
        snapshot.selected_events = [{"event_id": "evt_1", "content": "I changed 5 files in the PR"}]
        snapshot.code_context = []
        snapshot.documentation = []

        result = verifier.verify_claim(claim, snapshot)

        assert result.verified is True
        assert result.confidence > 0.9
        assert len(result.evidence) == 1
        assert result.evidence[0].source_type == "event"
        assert result.evidence[0].source_id == "evt_1"

    def test_verify_claim_with_code_evidence(self):
        """Test verification with evidence from code."""
        llm_client = Mock()
        llm_client.generate.return_value = json.dumps(
            {"verified": True, "confidence": 0.98, "reasoning": "Found in code"}
        )

        verifier = StructuredVerifier(llm_client)

        claim = Claim(
            type="factual",
            value="function is async",
            context="The function is async",
            position=20,
        )

        snapshot = Mock()
        snapshot.selected_events = []
        snapshot.code_context = [
            {
                "file_path": "src/main.py",
                "content": "async def process():\n    # function is async\n    pass",
            }
        ]
        snapshot.documentation = []

        result = verifier.verify_claim(claim, snapshot)

        assert result.verified is True
        assert len(result.evidence) == 1
        assert result.evidence[0].source_type == "code"
        assert "src/main.py" in result.evidence[0].source_id

    def test_verify_claim_no_evidence(self):
        """Test verification when no evidence found."""
        llm_client = Mock()
        verifier = StructuredVerifier(llm_client)

        claim = Claim(type="numeric", value="100 tests", context="Ran 100 tests", position=5)

        snapshot = Mock()
        snapshot.selected_events = []
        snapshot.code_context = []
        snapshot.documentation = []

        result = verifier.verify_claim(claim, snapshot)

        assert result.verified is False
        assert result.confidence == 0.0
        assert len(result.evidence) == 0
        assert "No evidence found" in result.contradiction


class TestEnhancedFirewall:
    """Test enhanced hallucination firewall."""

    def test_verify_with_llm_extraction(self):
        """Test verification using LLM extraction."""
        db = Mock()
        context_manager = Mock()
        llm_client = Mock()

        # Mock LLM extraction
        llm_client.generate.side_effect = [
            # First call: claim extraction
            json.dumps(
                [
                    {
                        "type": "numeric",
                        "value": "3 PRs",
                        "context": "Merged 3 PRs",
                        "position": 0,
                    }
                ]
            ),
            # Second call: semantic verification
            json.dumps({"verified": True, "confidence": 0.95, "reasoning": "Found in events"}),
        ]

        # Mock snapshot
        snapshot = Mock()
        snapshot.selected_events = [{"event_id": "evt_1", "content": "I merged 3 PRs today"}]
        snapshot.code_context = []
        snapshot.documentation = []

        context_manager.load_snapshot.return_value = snapshot

        firewall = HallucinationFirewall(
            db_factory=lambda: db,
            context_manager=context_manager,
            llm_client=llm_client,
            use_llm_extraction=True,
        )

        result = firewall.verify_response("Merged 3 PRs today", "snap_123")

        assert result.safe_to_deliver is True
        assert result.claims_verified == 1
        assert result.claims_failed == 0
        # Multi-dimensional: claim_verifiability(1.0)*0.45 + context_coverage*0.30 + freshness*0.25
        assert result.confidence_score >= 0.7
        assert result.evidence_count > 0

    def test_verify_with_regex_fallback(self):
        """Test verification using regex fallback."""
        db = Mock()
        context_manager = Mock()

        snapshot = Mock()
        snapshot.selected_events = [{"event_id": "evt_1", "content": "Changed 5 files"}]
        snapshot.code_context = []
        snapshot.documentation = []

        context_manager.load_snapshot.return_value = snapshot

        firewall = HallucinationFirewall(
            db_factory=lambda: db,
            context_manager=context_manager,
            llm_client=None,  # No LLM client
            use_llm_extraction=False,
        )

        result = firewall.verify_response("I changed 5 files", "snap_123")

        assert result.safe_to_deliver is True
        assert result.claims_verified >= 0

    def test_log_verification_with_evidence(self):
        """Test logging verification with evidence backlinks."""
        db = Mock()
        context_manager = Mock()
        llm_client = Mock()

        firewall = HallucinationFirewall(
            db_factory=lambda: db, context_manager=context_manager, llm_client=llm_client
        )

        # Create result with evidence
        from core.verification.structured_verifier import Evidence

        claim = Claim(type="numeric", value="5 files", context="", position=0)
        evidence = Evidence(
            source_type="event",
            source_id="evt_1",
            content="Changed 5 files",
            location="event:evt_1",
            confidence=0.9,
        )

        result = FirewallResult(
            safe_to_deliver=True,
            confidence_score=0.95,
            claims_verified=1,
            claims_failed=0,
            contradictions=[
                VerificationResult(
                    claim=claim,
                    verified=False,
                    confidence=0.5,
                    evidence=[evidence],
                    contradiction="Test",
                )
            ],
            warnings=[],
            evidence_count=1,
        )

        # Patch thread pool to run synchronously so we can assert
        from unittest.mock import patch

        with patch("core.verification.firewall._fw_pool") as mock_pool:
            mock_pool.submit.side_effect = lambda fn, *a, **kw: fn(*a, **kw)
            firewall.log_verification("sess_1", "evt_1", result, "snap_1")

        # ORM path: db.add() for HallucinationCheck + ClaimEvidence, then commit
        assert db.add.call_count >= 2
        assert db.commit.call_count >= 1


class TestEvidenceBacklinks:
    """Test evidence backlink functionality."""

    def test_evidence_traceability(self):
        """Test that evidence can be traced back to source."""
        evidence = Evidence(
            source_type="code",
            source_id="src/main.py",
            content="async def process():",
            location="src/main.py:42",
            confidence=0.95,
        )

        assert evidence.source_type == "code"
        assert "main.py" in evidence.source_id
        assert "42" in evidence.location
        assert evidence.confidence > 0.9

    def test_multiple_evidence_sources(self):
        """Test claim with evidence from multiple sources."""
        evidence_list = [
            Evidence(
                source_type="event",
                source_id="evt_1",
                content="Changed 5 files",
                location="event:evt_1",
                confidence=0.9,
            ),
            Evidence(
                source_type="code",
                source_id="src/stats.py",
                content="files_changed = 5",
                location="src/stats.py:10",
                confidence=0.95,
            ),
        ]

        assert len(evidence_list) == 2
        assert evidence_list[0].source_type == "event"
        assert evidence_list[1].source_type == "code"
        assert all(e.confidence > 0.8 for e in evidence_list)
