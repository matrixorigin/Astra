"""Tests for ImplicitFeedbackMiner."""

from unittest.mock import Mock, patch, MagicMock

from core.context.implicit_feedback import ImplicitFeedbackMiner


class TestAnalyzeAndStore:
    """Tests for analyze_and_store feedback storage."""

    def test_stores_positive_feedback(self):
        """Positive ratings (4-5) should also be stored, not just negative."""
        db = Mock()
        miner = ImplicitFeedbackMiner(db=db)

        # Mock analyze_batch to return one positive result
        positive = {
            "event_id": "evt_1",
            "session_id": "s1",
            "signal_type": "positive",
            "confidence": 0.9,
            "evidence": "thanks!",
            "rating": 5,
            "user_followup": "thanks!",
        }
        with patch.object(miner, "analyze_batch", return_value=[positive]):
            with patch("core.context.prompts.PromptFeedback") as MockPF:
                pf_instance = MockPF.return_value
                count = miner.analyze_and_store(session_id="s1")

        assert count == 1
        pf_instance.record_feedback.assert_called_once()
        call_kwargs = pf_instance.record_feedback.call_args
        assert call_kwargs[1]["user_rating"] == 5

    def test_stores_both_positive_and_negative(self):
        """Both positive and negative feedback should be stored."""
        db = Mock()
        miner = ImplicitFeedbackMiner(db=db)

        results = [
            {"event_id": "e1", "session_id": "s1", "signal_type": "correction",
             "confidence": 0.8, "evidence": "wrong", "rating": 1, "user_followup": "no"},
            {"event_id": "e2", "session_id": "s1", "signal_type": "positive",
             "confidence": 0.9, "evidence": "great", "rating": 5, "user_followup": "thanks"},
        ]
        with patch.object(miner, "analyze_batch", return_value=results):
            with patch("core.context.prompts.PromptFeedback") as MockPF:
                count = miner.analyze_and_store(session_id="s1")

        assert count == 2
