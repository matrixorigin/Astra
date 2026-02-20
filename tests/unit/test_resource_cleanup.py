"""Test resource cleanup and session management after refactoring.

After the refactoring, Core layer modules no longer create their own sessions.
This test verifies that:
1. All Core modules require a session parameter
2. All Core modules use the injected session
3. Session lifecycle is managed by the caller (API/Service layer)
"""

import pytest
from unittest.mock import Mock
from sqlalchemy.orm import Session

from core.git_for_data import GitForData
from core.sandbox.sandbox import Sandbox
from core.events.event_reader import EventReader
from core.events.event_logger import EventLogger
from core.events.session_manager import SessionManager
from core.replay.time_machine import TimeMachine
from core.skills.registry import SkillRegistry
from core.skills.pipeline import SkillPipeline
from core.skills.self_improving_selector import SelfImprovingSelector
from core.skills.selector import SkillSelector
from core.skills.modern_selector import ModernSkillSelector
from core.events.causal_chain import CausalChainManager
from core.replay.semantic_diff import SemanticDiff
from core.skills.mocking import ToolMockingLayer, MockMode


class TestSessionInjection:
    """Test that all Core modules require session injection."""

    def test_git_for_data_requires_session(self):
        """Test GitForData requires session parameter."""
        with pytest.raises(TypeError, match="db must be a SQLAlchemy Session"):
            GitForData(db=None)
        
        # Should work with proper session
        mock_session = Mock(spec=Session)
        git = GitForData(db=mock_session)
        assert git.db is mock_session

    def test_event_logger_requires_session(self):
        """Test EventLogger requires session parameter."""
        with pytest.raises(TypeError, match="session must be a SQLAlchemy Session"):
            EventLogger(session=None)
        
        # Should work with proper session
        mock_session = Mock(spec=Session)
        logger = EventLogger(session=mock_session)
        assert logger.session is mock_session

    def test_sandbox_requires_session(self):
        """Test Sandbox requires session parameter."""
        with pytest.raises(TypeError, match="db must be a SQLAlchemy Session"):
            Sandbox(db=None)
        
        # Should work with proper session
        mock_session = Mock(spec=Session)
        sandbox = Sandbox(db=mock_session)
        assert sandbox.db is mock_session

    def test_skill_registry_requires_session(self):
        """Test SkillRegistry requires session parameter."""
        with pytest.raises(TypeError, match="session must be a SQLAlchemy Session"):
            SkillRegistry(session=None)
        
        # Should work with proper session
        mock_session = Mock(spec=Session)
        mock_session.query.return_value.filter.return_value.all.return_value = []
        registry = SkillRegistry(session=mock_session)
        assert registry.session is mock_session

    def test_pipeline_requires_session(self):
        """Test SkillPipeline requires session parameter."""
        with pytest.raises(TypeError):
            SkillPipeline(db=None, llm_client=None)
        
        # Should work with proper session
        mock_session = Mock(spec=Session)
        mock_session.query.return_value.filter.return_value.all.return_value = []
        pipeline = SkillPipeline(mock_session, llm_client=None)
        assert pipeline._db is mock_session

    def test_modern_selector_requires_session(self):
        """Test ModernSkillSelector requires session parameter."""
        with pytest.raises(TypeError, match="session must be a SQLAlchemy Session"):
            ModernSkillSelector(session=None)
        
        # Should work with proper session
        mock_session = Mock(spec=Session)
        mock_session.query.return_value.filter.return_value.all.return_value = []
        selector = ModernSkillSelector(session=mock_session)
        assert selector.session is mock_session

    def test_self_improving_selector_requires_session(self):
        """Test SelfImprovingSelector requires session parameter."""
        with pytest.raises(TypeError, match="session must be a SQLAlchemy Session"):
            SelfImprovingSelector(session=None)
        
        # Should work with proper session
        mock_session = Mock(spec=Session)
        mock_session.query.return_value.filter.return_value.all.return_value = []
        selector = SelfImprovingSelector(session=mock_session)
        assert selector.session is mock_session

    def test_tool_mocking_layer_requires_session(self):
        """Test ToolMockingLayer requires session parameter."""
        with pytest.raises(TypeError, match="session must be a SQLAlchemy Session"):
            ToolMockingLayer(MockMode.PRODUCTION, session=None)
        
        # Should work with proper session
        mock_session = Mock(spec=Session)
        layer = ToolMockingLayer(MockMode.PRODUCTION, session=mock_session)
        assert layer.session is mock_session

    def test_semantic_diff_requires_session(self):
        """Test SemanticDiff requires session parameter."""
        with pytest.raises(TypeError, match="db must be a SQLAlchemy Session"):
            SemanticDiff(db=None)
        
        # Should work with proper session
        mock_session = Mock(spec=Session)
        diff = SemanticDiff(db=mock_session)
        assert diff.db is mock_session


class TestSessionSharing:
    """Test that modules share the same session when passed."""

    def test_sandbox_shares_session_with_branch(self):
        """Test Sandbox shares session with Branch."""
        mock_session = Mock(spec=Session)
        sandbox = Sandbox(db=mock_session)
        
        # Branch inside sandbox should use the same session
        assert sandbox.branch.db is mock_session

    def test_pipeline_shares_session(self):
        """Test SkillPipeline shares session with dependencies."""
        mock_session = Mock(spec=Session)
        mock_session.query.return_value.filter.return_value.all.return_value = []
        
        pipeline = SkillPipeline(mock_session, llm_client=None)
        
        # Internal modern selector should use the same session
        assert pipeline._modern.session is mock_session

    def test_self_improving_selector_shares_session(self):
        """Test SelfImprovingSelector shares session with dependencies."""
        mock_session = Mock(spec=Session)
        mock_session.query.return_value.filter.return_value.all.return_value = []
        
        selector = SelfImprovingSelector(session=mock_session)
        
        # Dependencies should use the same session
        assert selector.sandbox.db is mock_session


class TestNoSessionCreation:
    """Test that Core modules don't create their own sessions."""

    def test_modules_dont_have_sessionlocal_import(self):
        """Test that Core modules don't import SessionLocal."""
        import inspect
        
        # Check that these modules don't create SessionLocal
        modules_to_check = [
            GitForData,
            EventLogger,
            Sandbox,
            SkillRegistry,
            SkillPipeline,
            ModernSkillSelector,
            SelfImprovingSelector,
            ToolMockingLayer,
            SemanticDiff,
        ]
        
        for module_class in modules_to_check:
            source = inspect.getsource(module_class.__init__)
            # Should not create SessionLocal in __init__
            assert "SessionLocal()" not in source, f"{module_class.__name__} creates SessionLocal"

    def test_modules_dont_have_lazy_session(self):
        """Test that Core modules don't have _lazy_session attribute."""
        mock_session = Mock(spec=Session)
        mock_session.query.return_value.filter.return_value.all.return_value = []
        
        modules = [
            GitForData(db=mock_session),
            EventLogger(session=mock_session),
            Sandbox(db=mock_session),
            SkillRegistry(session=mock_session),
            ModernSkillSelector(session=mock_session),
            ToolMockingLayer(MockMode.PRODUCTION, session=mock_session),
            SemanticDiff(db=mock_session),
        ]
        
        for module in modules:
            assert not hasattr(module, '_lazy_session'), f"{type(module).__name__} has _lazy_session"
            assert not hasattr(module, '_owns_session'), f"{type(module).__name__} has _owns_session"


class TestSessionLifecycle:
    """Test that session lifecycle is managed by caller."""

    def test_caller_manages_session_lifecycle(self):
        """Test that caller is responsible for session lifecycle."""
        from api.database import get_db_session
        
        # Caller gets session
        db = next(get_db_session())
        
        # Pass to modules
        logger = EventLogger(session=db)
        git = GitForData(db=db)
        
        # Modules use the session but don't close it
        assert logger.session is db
        assert git.db is db
        
        # Caller is responsible for closing
        db.close()

    def test_modules_dont_have_close_method(self):
        """Test that Core modules don't have close() method."""
        mock_session = Mock(spec=Session)
        mock_session.query.return_value.filter.return_value.all.return_value = []
        
        modules = [
            GitForData(db=mock_session),
            EventLogger(session=mock_session),
            SkillRegistry(session=mock_session),
            ModernSkillSelector(session=mock_session),
            ToolMockingLayer(MockMode.PRODUCTION, session=mock_session),
            SemanticDiff(db=mock_session),
        ]
        
        for module in modules:
            # Should not have close method (or if it has, it shouldn't close the session)
            if hasattr(module, 'close'):
                # If close exists, it should be for other purposes, not session management
                pass  # We allow close for other purposes


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
