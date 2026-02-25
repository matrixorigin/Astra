"""Integration tests for enhanced hallucination firewall."""

import json
from unittest.mock import Mock

import pytest

from core.verification.firewall import HallucinationFirewall


class TestEnhancedFirewallIntegration:
    """Integration tests for enhanced firewall with real-like scenarios."""

    def test_end_to_end_verification_flow(self):
        """Test complete verification flow from extraction to logging."""
        # Setup
        db = Mock()
        context_manager = Mock()
        llm_client = Mock()

        # Mock LLM responses
        llm_client.generate.side_effect = [
            # Claim extraction
            json.dumps(
                [
                    {
                        "type": "numeric",
                        "value": "5 files changed",
                        "context": "I changed 5 files",
                        "position": 0,
                    },
                    {
                        "type": "causal",
                        "value": "test fails because timeout",
                        "context": "The test fails because timeout",
                        "position": 50,
                    },
                ]
            ),
            # Verification for claim 1
            json.dumps(
                {"verified": True, "confidence": 0.95, "reasoning": "Found in events"}
            ),
            # Verification for claim 2
            json.dumps(
                {
                    "verified": False,
                    "confidence": 0.3,
                    "reasoning": "No timeout mentioned",
                }
            ),
        ]

        # Mock snapshot
        snapshot = Mock()
        snapshot.selected_events = [
            {
                "event_id": "evt_1",
                "content": "I changed 5 files in the last commit",
            },
            {"event_id": "evt_2", "content": "All tests passed successfully"},
        ]
        snapshot.code_context = []
        snapshot.documentation = []

        context_manager.load_snapshot.return_value = snapshot

        # Create firewall
        firewall = HallucinationFirewall(
            db_factory=lambda: db,
            context_manager=context_manager,
            llm_client=llm_client,
            threshold=0.7,
            use_llm_extraction=True,
        )

        # Execute verification
        response = "I changed 5 files. The test fails because timeout."
        result = firewall.verify_response(response, "snap_123", mode="warn")

        # Assertions
        assert result.claims_verified == 1
        assert result.claims_failed == 1
        # Multi-dimensional confidence: claim_verifiability is low (0.333),
        # blended with context_coverage and freshness
        assert result.confidence_score < 0.7  # should be below threshold
        assert result.safe_to_deliver is True  # warn mode
        assert result.evidence_count > 0

        # Log verification
        firewall.log_verification("sess_1", "evt_1", result, "snap_123")

        # Verify database interactions
        # Note: Only 1 insert for hallucination_checks (no contradictions with evidence)
        assert db.execute.call_count >= 1
        db.commit.assert_called()

    def test_block_mode_with_low_confidence(self):
        """Test that block mode rejects low confidence responses."""
        db = Mock()
        context_manager = Mock()
        llm_client = Mock()

        # Mock low confidence verification
        llm_client.generate.side_effect = [
            json.dumps(
                [
                    {
                        "type": "numeric",
                        "value": "100 tests",
                        "context": "Ran 100 tests",
                        "position": 0,
                    }
                ]
            ),
            json.dumps(
                {
                    "verified": False,
                    "confidence": 0.2,
                    "reasoning": "No evidence found",
                }
            ),
        ]

        snapshot = Mock()
        snapshot.selected_events = [{"event_id": "evt_1", "content": "Some content"}]
        snapshot.code_context = []
        snapshot.documentation = []

        context_manager.load_snapshot.return_value = snapshot

        firewall = HallucinationFirewall(
            db_factory=lambda: db,
            context_manager=context_manager,
            llm_client=llm_client,
            threshold=0.7,
            use_llm_extraction=True,
        )

        result = firewall.verify_response("Ran 100 tests", "snap_123", mode="block")

        assert result.safe_to_deliver is False  # Blocked
        assert result.confidence_score < 0.7
        assert "threshold" in result.warnings[0].lower()

    def test_multiple_evidence_sources(self):
        """Test verification with evidence from multiple sources."""
        db = Mock()
        context_manager = Mock()
        llm_client = Mock()

        llm_client.generate.side_effect = [
            json.dumps(
                [
                    {
                        "type": "factual",
                        "value": "function is async",
                        "context": "The function is async",
                        "position": 0,
                    }
                ]
            ),
            json.dumps(
                {"verified": True, "confidence": 0.98, "reasoning": "Multiple sources"}
            ),
        ]

        snapshot = Mock()
        snapshot.selected_events = [
            {"event_id": "evt_1", "content": "I made the function async"}
        ]
        snapshot.code_context = [
            {
                "file_path": "src/main.py",
                "content": "async def process():\n    pass",
            }
        ]
        snapshot.documentation = [
            {"doc_id": "doc_1", "content": "The process function is async"}
        ]

        context_manager.load_snapshot.return_value = snapshot

        firewall = HallucinationFirewall(
            db_factory=lambda: db,
            context_manager=context_manager,
            llm_client=llm_client,
            use_llm_extraction=True,
        )

        result = firewall.verify_response("The function is async", "snap_123")

        assert result.safe_to_deliver is True
        assert result.confidence_score > 0.7  # multi-dimensional, all claims verified
        assert result.evidence_count >= 1  # At least one evidence source

    def test_graceful_degradation_without_llm(self):
        """Test that firewall works without LLM client (regex fallback)."""
        db = Mock()
        context_manager = Mock()

        snapshot = Mock()
        snapshot.selected_events = [{"event_id": "evt_1", "content": "Changed 5 files"}]
        snapshot.code_context = []
        snapshot.documentation = []

        context_manager.load_snapshot.return_value = snapshot

        # No LLM client
        firewall = HallucinationFirewall(
            db_factory=lambda: db,
            context_manager=context_manager,
            llm_client=None,
            use_llm_extraction=False,
        )

        result = firewall.verify_response("I changed 5 files", "snap_123")

        # Should still work with regex extraction
        assert result.safe_to_deliver is True
        assert result.claims_verified >= 0

    def test_error_handling_in_verification(self):
        """Test error handling during verification."""
        db = Mock()
        context_manager = Mock()
        llm_client = Mock()

        # Mock LLM failure
        llm_client.generate.side_effect = Exception("LLM API error")

        snapshot = Mock()
        snapshot.selected_events = []
        snapshot.code_context = []
        snapshot.documentation = []
        context_manager.load_snapshot.return_value = snapshot

        firewall = HallucinationFirewall(
            db_factory=lambda: db,
            context_manager=context_manager,
            llm_client=llm_client,
            use_llm_extraction=True,
        )

        result = firewall.verify_response("Some text", "snap_123")

        # Should fail open
        assert result.safe_to_deliver is True
        assert "extraction failed" in result.warnings[0].lower() or "no verifiable" in result.warnings[0].lower()

    def test_snapshot_load_failure(self):
        """Test handling of snapshot load failures."""
        db = Mock()
        context_manager = Mock()
        llm_client = Mock()

        # Mock successful claim extraction
        llm_client.generate.return_value = json.dumps([
            {"type": "numeric", "value": "5 files", "context": "", "position": 0}
        ])

        # Mock snapshot load failure
        context_manager.load_snapshot.side_effect = Exception("Snapshot not found")

        firewall = HallucinationFirewall(
            db_factory=lambda: db, context_manager=context_manager, llm_client=llm_client
        )

        result = firewall.verify_response("Some text", "snap_123")

        # Should fail open
        assert result.safe_to_deliver is True
        assert "Context capture load failed" in result.warnings[0]
