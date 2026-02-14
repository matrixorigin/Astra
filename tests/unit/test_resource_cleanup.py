
import pytest
from unittest.mock import MagicMock, patch
from core.git_for_data import GitForData
from core.sandbox.sandbox import Sandbox
from core.events.event_reader import EventReader
from core.events.event_logger import EventLogger
from core.events.session_manager import SessionManager
from core.replay.time_machine import TimeMachine
from core.skills.registry import SkillRegistry
from core.skills.auditable_selector import AuditableSkillSelector
from core.skills.self_improving_selector import SelfImprovingSelector
from core.skills.regression_gate import SkillSelectionRegressionGate
from core.skills.selector import SkillSelector
from core.skills.modern_selector import ModernSkillSelector
from core.events.causal_chain import CausalChainManager
from core.replay.semantic_diff import SemanticDiff
from core.skills.mocking import ToolMockingLayer, MockMode

class TestResourceCleanup:
    """Test resource cleanup and session management."""

    @patch("core.git_for_data.SessionLocal")
    def test_git_for_data_context_manager(self, mock_session_local):
        """Test GitForData context manager closes session."""
        mock_session = MagicMock()
        mock_session_local.return_value = mock_session

        # Test with context manager
        with GitForData() as git:
            assert git._owns_session is True
            assert git.db is mock_session
        
        # Verify close was called
        mock_session.close.assert_called_once()

    @patch("core.events.event_logger.SessionLocal")
    def test_event_logger_context_manager(self, mock_session_local):
        """Test EventLogger context manager closes session."""
        mock_session = MagicMock()
        mock_session_local.return_value = mock_session

        with EventLogger() as logger:
            assert logger._owns_session is True
            assert logger.session is mock_session
            
        mock_session.close.assert_called_once()

    def test_event_logger_external_session(self):
        """Test EventLogger with external session doesn't close it."""
        mock_session = MagicMock()
        
        with EventLogger(session=mock_session) as logger:
            assert logger._owns_session is False
            assert logger.session is mock_session
            
        # Should NOT be called
        mock_session.close.assert_not_called()

    @patch("core.git_for_data.SessionLocal")
    def test_git_for_data_manual_close(self, mock_session_local):
        """Test GitForData manual close."""
        mock_session = MagicMock()
        mock_session_local.return_value = mock_session

        git = GitForData()
        # Trigger lazy session creation
        _ = git.db
        git.close()
        
        mock_session.close.assert_called_once()

    def test_git_for_data_external_session(self):
        """Test GitForData with external session doesn't close it."""
        mock_session = MagicMock()
        
        with GitForData(db=mock_session) as git:
            assert git._owns_session is False
            assert git.db is mock_session
            
        # Should NOT be called
        mock_session.close.assert_not_called()

    @patch("core.sandbox.sandbox.get_db_session")
    def test_sandbox_context_manager(self, mock_get_db_session):
        """Test Sandbox context manager closes session."""
        mock_session = MagicMock()
        mock_get_db_session.return_value = iter([mock_session])

        # Test with context manager
        with Sandbox() as sandbox:
            assert sandbox._owns_session is True
            assert sandbox.db is mock_session
            # GitForData inside sandbox should share the session
            assert sandbox.git.db is mock_session
            assert sandbox.git._owns_session is False # It shares session, so it doesn't own it
        
        # Verify close was called
        mock_session.close.assert_called_once()

    @patch("core.sandbox.sandbox.get_db_session")
    def test_sandbox_manual_close(self, mock_get_db_session):
        """Test Sandbox manual close."""
        mock_session = MagicMock()
        mock_get_db_session.return_value = iter([mock_session])

        sandbox = Sandbox()
        sandbox.close()
        
        mock_session.close.assert_called_once()

    def test_sandbox_external_session(self):
        """Test Sandbox with external session doesn't close it."""
        mock_session = MagicMock()
        
        with Sandbox(db=mock_session) as sandbox:
            assert sandbox._owns_session is False
            assert sandbox.db is mock_session
            
        # Should NOT be called
        mock_session.close.assert_not_called()

    @patch("core.events.event_reader.get_db_session")
    def test_event_reader_context_manager(self, mock_get_db_session):
        """Test EventReader context manager closes session."""
        mock_session = MagicMock()
        mock_get_db_session.return_value = iter([mock_session])

        with EventReader() as reader:
            assert reader._owns_session is True
            assert reader.db is mock_session
            
        mock_session.close.assert_called_once()

    @patch("core.replay.time_machine.get_db_session")
    def test_time_machine_context_manager(self, mock_get_db_session):
        """Test TimeMachine context manager closes session."""
        mock_session = MagicMock()
        mock_get_db_session.return_value = iter([mock_session])

        with TimeMachine() as tm:
            assert tm._owns_session is True
            assert tm.db is mock_session
            # Dependencies should share session
            assert tm.git.db is mock_session
            assert tm.reader.db is mock_session
            assert tm.git._owns_session is False
            assert tm.reader._owns_session is False
            
        mock_session.close.assert_called_once()

    @patch("core.events.session_manager.get_db_session")
    def test_session_manager_context_manager(self, mock_get_db_session):
        """Test SessionManager context manager closes session."""
        mock_session = MagicMock()
        mock_get_db_session.return_value = iter([mock_session])

        with SessionManager() as manager:
            # Trigger session creation
            manager._get_session()
            assert manager._owns_session is True
            assert manager._session is mock_session
            
        mock_session.close.assert_called_once()

    @patch("core.skills.registry.SessionLocal")
    def test_skill_registry_context_manager(self, mock_session_local):
        """Test SkillRegistry context manager closes session."""
        mock_session = MagicMock()
        mock_session_local.return_value = mock_session

        with SkillRegistry() as registry:
            assert registry._owns_session is True
            assert registry.session is mock_session
            
        mock_session.close.assert_called_once()

    @patch("core.skills.auditable_selector.SessionLocal")
    @patch("core.skills.auditable_selector.AuditableSkillSelector._ensure_table")
    def test_auditable_selector_context_manager(self, mock_ensure, mock_session_local):
        """Test AuditableSkillSelector context manager closes session."""
        mock_session = MagicMock()
        mock_session_local.return_value = mock_session

        with AuditableSkillSelector() as selector:
            assert selector._owns_session is True
            assert selector.session is mock_session
            assert selector.modern_selector.session is mock_session
            assert selector.modern_selector._owns_session is False
            assert selector.sandbox.db is mock_session
            assert selector.sandbox._owns_session is False
            
        mock_session.close.assert_called_once()

    @patch("core.skills.self_improving_selector.SessionLocal")
    @patch("core.skills.self_improving_selector.SelfImprovingSelector._ensure_tables")
    @patch("core.skills.auditable_selector.AuditableSkillSelector._ensure_table")
    def test_self_improving_selector_context_manager(self, mock_ensure1, mock_ensure2, mock_session_local):
        """Test SelfImprovingSelector context manager closes session."""
        mock_session = MagicMock()
        mock_session_local.return_value = mock_session

        with SelfImprovingSelector() as selector:
            assert selector._owns_session is True
            assert selector.session is mock_session
            assert selector.auditable_selector.session is mock_session
            assert selector.auditable_selector._owns_session is False
            assert selector.sandbox.db is mock_session
            assert selector.sandbox._owns_session is False
            
        mock_session.close.assert_called_once()

    @patch("core.skills.regression_gate.SessionLocal")
    @patch("core.skills.regression_gate.SkillSelectionRegressionGate._ensure_tables")
    def test_regression_gate_context_manager(self, mock_ensure, mock_session_local):
        """Test SkillSelectionRegressionGate context manager closes session."""
        mock_session = MagicMock()
        mock_session_local.return_value = mock_session

        with SkillSelectionRegressionGate(llm_client=None) as gate:
            assert gate._owns_session is True
            assert gate.session is mock_session
            assert gate.sandbox.db is mock_session
            assert gate.sandbox._owns_session is False
            
        mock_session.close.assert_called_once()

    @patch("core.skills.selector.get_db_session")
    @patch("core.skills.selector.SkillSelector._load_skills")
    def test_skill_selector_context_manager(self, mock_load, mock_get_db_session):
        """Test SkillSelector context manager closes session."""
        mock_session = MagicMock()
        mock_get_db_session.return_value = iter([mock_session])

        with SkillSelector() as selector:
            # Trigger session creation
            selector._get_session()
            assert selector._owns_session is True
            assert selector._session is mock_session
            
        mock_session.close.assert_called_once()

    @patch("core.skills.modern_selector.SessionLocal")
    @patch("core.skills.selector.SkillSelector._load_skills")
    def test_modern_skill_selector_context_manager(self, mock_load, mock_session_local):
        """Test ModernSkillSelector context manager closes session."""
        mock_session = MagicMock()
        mock_session_local.return_value = mock_session

        with ModernSkillSelector() as selector:
            assert selector._owns_session is True
            assert selector.session is mock_session
            assert selector.rule_selector._session is mock_session
            assert selector.rule_selector._owns_session is False
            
        mock_session.close.assert_called_once()

    @patch("core.events.causal_chain.get_db_session")
    def test_causal_chain_manager_context_manager(self, mock_get_db_session):
        """Test CausalChainManager context manager closes session."""
        mock_session = MagicMock()
        mock_get_db_session.return_value = iter([mock_session])

        with CausalChainManager() as manager:
            assert manager._owns_session is True
            assert manager.db is mock_session
            assert manager.reader.db is mock_session
            assert manager.reader._owns_session is False
            
        mock_session.close.assert_called_once()

    @patch("core.replay.semantic_diff.SessionLocal")
    def test_semantic_diff_context_manager(self, mock_session_local):
        """Test SemanticDiff context manager closes session."""
        mock_session = MagicMock()
        mock_session_local.return_value = mock_session

        with SemanticDiff() as diff:
            assert diff._owns_session is True
            assert diff.db is mock_session
            assert diff.reader.db is mock_session
            assert diff.reader._owns_session is False
            
        mock_session.close.assert_called_once()

    @patch("core.skills.mocking.SessionLocal")
    def test_tool_mocking_layer_context_manager(self, mock_session_local):
        """Test ToolMockingLayer context manager closes session."""
        mock_session = MagicMock()
        mock_session_local.return_value = mock_session

        with ToolMockingLayer(MockMode.PRODUCTION) as layer:
            # Trigger session creation
            session = layer._get_session()
            assert layer._owns_session is True
            assert session is mock_session
            
        mock_session.close.assert_called_once()

    def test_tool_mocking_layer_external_session(self):
        """Test ToolMockingLayer with external session doesn't close it."""
        mock_session = MagicMock()
        
        with ToolMockingLayer(MockMode.PRODUCTION, session=mock_session) as layer:
            assert layer._owns_session is False
            assert layer._session is mock_session
            
        # Should NOT be called
        mock_session.close.assert_not_called()
