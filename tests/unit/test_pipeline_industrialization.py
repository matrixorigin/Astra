"""Tests for multi-agent collaboration industrialization fixes.

Covers:
- Rollback table name fix (selector_learnings → skill_selection_learning)
- Learning correction order preservation
- Exact-name skill backfill (no semantic drift)
- FeedbackBuffer thread-safe flush
"""

import json
import threading
import time
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from unittest.mock import Mock, MagicMock, patch

import pytest
from uuid_utils import uuid7


# ---------------------------------------------------------------------------
# Fix 1: Rollback delegates to SelfImprovingSelector (correct table + soft-delete)
# ---------------------------------------------------------------------------

class TestRollbackDelegation:

    def test_rollback_calls_improver_with_correct_since(self):
        """_rollback_learnings delegates to SelfImprovingSelector.rollback_learnings."""
        from core.skills.pipeline import SkillPipeline

        mock_db = Mock()
        mock_llm = Mock()
        pipeline = SkillPipeline.__new__(SkillPipeline)
        pipeline._db = mock_db

        mock_improver = Mock()
        mock_improver.rollback_learnings.return_value = 3
        pipeline._improver = mock_improver

        pipeline._rollback_learnings(days=7)

        mock_improver.rollback_learnings.assert_called_once()
        since = mock_improver.rollback_learnings.call_args[1]["since"]
        expected = datetime.now(timezone.utc) - timedelta(days=7)
        assert abs((since - expected).total_seconds()) < 5

    def test_rollback_noop_without_improver(self):
        """No crash when learning is disabled."""
        from core.skills.pipeline import SkillPipeline

        pipeline = SkillPipeline.__new__(SkillPipeline)
        pipeline._db = Mock()
        pipeline._improver = None

        pipeline._rollback_learnings(days=7)  # should not raise

    def test_rollback_handles_exception(self):
        """Exception in rollback is caught, not propagated."""
        from core.skills.pipeline import SkillPipeline

        pipeline = SkillPipeline.__new__(SkillPipeline)
        pipeline._db = Mock()
        mock_improver = Mock()
        mock_improver.rollback_learnings.side_effect = RuntimeError("db down")
        pipeline._improver = mock_improver

        pipeline._rollback_learnings(days=7)  # should not raise
        pipeline._db.rollback.assert_called_once()


# ---------------------------------------------------------------------------
# Fix 2: Learning correction preserves order
# ---------------------------------------------------------------------------

class TestCorrectionOrderPreservation:

    def _make_pipeline_with_tools(self, original_tools, corrected_candidates):
        """Build a SkillPipeline with mocked internals."""
        from core.skills.pipeline import SkillPipeline, SkillCandidate

        pipeline = SkillPipeline.__new__(SkillPipeline)
        pipeline._db = Mock()
        pipeline._audit = False
        pipeline._learning = True

        mock_modern = Mock()
        mock_modern.get_tools_schema.return_value = (original_tools, "keyword")
        mock_modern._skill_to_tool_schema_by_name = Mock(return_value=None)
        pipeline._modern = mock_modern

        mock_improver = Mock()
        mock_improver.apply_learnings.return_value = corrected_candidates
        pipeline._improver = mock_improver

        pipeline._feedback = Mock()
        pipeline._feedback.maybe_flush.return_value = 0

        return pipeline

    def test_corrected_order_preserved(self):
        """Tools are reordered to match apply_learnings output order."""
        from core.skills.pipeline import SkillCandidate

        tools = [
            {"type": "function", "function": {"name": "alpha"}},
            {"type": "function", "function": {"name": "beta"}},
            {"type": "function", "function": {"name": "gamma"}},
        ]
        # Correction reorders: gamma first, alpha second, beta removed
        corrected = [
            SkillCandidate(name="gamma", confidence=0.9),
            SkillCandidate(name="alpha", confidence=0.5),
        ]

        pipeline = self._make_pipeline_with_tools(tools, corrected)
        result = pipeline.get_tools_schema("test query", "sess1")

        names = [t["function"]["name"] for t in result.tools]
        assert names == ["gamma", "alpha"]

    def test_new_skill_added_by_correction(self):
        """Correction adds a skill not in original candidates."""
        from core.skills.pipeline import SkillCandidate

        tools = [{"type": "function", "function": {"name": "alpha"}}]
        corrected = [
            SkillCandidate(name="alpha"),
            SkillCandidate(name="new_skill"),
        ]

        pipeline = self._make_pipeline_with_tools(tools, corrected)
        # Mock exact-name lookup for the new skill
        new_schema = {"type": "function", "function": {"name": "new_skill", "description": "new"}}
        pipeline._modern._skill_to_tool_schema_by_name = Mock(return_value=new_schema)

        result = pipeline.get_tools_schema("test query", "sess1")
        names = [t["function"]["name"] for t in result.tools]
        assert names == ["alpha", "new_skill"]

    def test_new_skill_not_in_registry_skipped(self):
        """Correction adds a skill that doesn't exist in registry — silently skipped."""
        from core.skills.pipeline import SkillCandidate

        tools = [{"type": "function", "function": {"name": "alpha"}}]
        corrected = [
            SkillCandidate(name="alpha"),
            SkillCandidate(name="ghost_skill"),
        ]

        pipeline = self._make_pipeline_with_tools(tools, corrected)
        pipeline._modern._skill_to_tool_schema_by_name = Mock(return_value=None)

        result = pipeline.get_tools_schema("test query", "sess1")
        names = [t["function"]["name"] for t in result.tools]
        assert names == ["alpha"]  # ghost_skill silently dropped


# ---------------------------------------------------------------------------
# Fix 3: Exact-name lookup (no semantic drift)
# ---------------------------------------------------------------------------

class TestExactNameLookup:

    def test_skill_to_tool_schema_by_name_found(self):
        """Exact name match returns schema."""
        from core.skills.modern_selector import ModernSkillSelector

        mock_db = Mock(spec=["__class__"])
        mock_db.__class__ = __import__("sqlalchemy.orm", fromlist=["Session"]).Session

        selector = ModernSkillSelector.__new__(ModernSkillSelector)
        selector.rule_selector = Mock()
        selector._registry = Mock()

        mock_skill = Mock()
        mock_skill.name = "deploy"
        mock_skill.description = "Deploy service"
        selector.rule_selector.skills = {"deploy": mock_skill}

        # Mock _skill_to_tool_schema to return a known value
        with patch.object(ModernSkillSelector, '_skill_to_tool_schema',
                         return_value={"type": "function", "function": {"name": "deploy"}}):
            result = selector._skill_to_tool_schema_by_name("deploy")
        assert result is not None
        assert result["function"]["name"] == "deploy"

    def test_skill_to_tool_schema_by_name_not_found(self):
        """Unknown name returns None, no semantic search."""
        from core.skills.modern_selector import ModernSkillSelector

        selector = ModernSkillSelector.__new__(ModernSkillSelector)
        selector.rule_selector = Mock()
        selector.rule_selector.skills = {}

        result = selector._skill_to_tool_schema_by_name("nonexistent")
        assert result is None


# ---------------------------------------------------------------------------
# Fix 4: FeedbackBuffer uses independent connection
# ---------------------------------------------------------------------------

class TestFeedbackBufferThreadSafety:

    def test_flush_uses_independent_connection(self):
        """Flush creates its own connection, not the shared Session."""
        from core.skills.pipeline import _FeedbackBuffer
        from core.skills.learning_signals import SignalType

        mock_conn = MagicMock()
        mock_conn.__enter__ = Mock(return_value=mock_conn)
        mock_conn.__exit__ = Mock(return_value=False)

        mock_engine = Mock()
        mock_engine.connect.return_value = mock_conn

        mock_bind = Mock()
        mock_bind.connect.return_value = mock_conn
        mock_bind.engine = mock_engine

        mock_db = Mock()
        mock_db.get_bind.return_value = mock_bind

        buf = _FeedbackBuffer(mock_db, batch_size=100)
        buf.add("evt1", SignalType.WRONG_SKILL, {"skill": "bad"})

        count = buf.flush()

        # Should have used get_bind().connect(), not self._db.execute()
        assert mock_db.get_bind.called
        assert mock_bind.connect.called
        assert count == 1
        # The shared session should NOT have been used for execute
        mock_db.execute.assert_not_called()
        mock_db.commit.assert_not_called()

    def test_flush_requeues_on_failure(self):
        """Failed flush re-queues items for retry."""
        from core.skills.pipeline import _FeedbackBuffer
        from core.skills.learning_signals import SignalType

        mock_conn = MagicMock()
        mock_conn.__enter__ = Mock(return_value=mock_conn)
        mock_conn.__exit__ = Mock(return_value=False)
        mock_conn.execute.side_effect = RuntimeError("connection lost")

        mock_bind = Mock()
        mock_bind.connect.return_value = mock_conn

        mock_db = Mock()
        mock_db.get_bind.return_value = mock_bind

        buf = _FeedbackBuffer(mock_db, batch_size=100)
        buf.add("evt1", SignalType.WRONG_SKILL, {"skill": "bad"})

        count = buf.flush()
        assert count == 0
        assert len(buf._buffer) == 1  # re-queued
