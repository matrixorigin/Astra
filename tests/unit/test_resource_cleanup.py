"""Test resource cleanup and session management after DbConsumer migration.

Core layer DbConsumer modules use db_factory (callable) instead of raw Session.
Non-DbConsumer modules (SkillRegistry, ModernSkillSelector, SkillPipeline) still
use db_factory but without DbConsumer base class.

This test verifies:
1. DbConsumer modules require callable db_factory
2. Non-DbConsumer modules with db_factory validate the argument
3. No module creates its own SessionLocal
4. DbConsumer modules share the same factory with children
"""

import pytest
from unittest.mock import Mock
from sqlalchemy.orm import Session

from core.git_for_data import GitForData
from core.sandbox.sandbox import Sandbox
from core.events.event_logger import EventLogger
from core.skills.registry import SkillRegistry
from core.skills.pipeline import SkillPipeline
from core.skills.self_improving_selector import SelfImprovingSelector
from core.skills.modern_selector import ModernSkillSelector
from core.replay.semantic_diff import SemanticDiff
from core.skills.mocking import ToolMockingLayer, MockMode
from core.skills.skill_manager import SkillManager
from core.skills.credential_manager import CredentialManager
from core.db_consumer import DbConsumer


class TestSessionInjection:
    """Test that DbConsumer modules require callable db_factory."""

    def test_git_for_data_requires_session(self):
        with pytest.raises(TypeError, match="db_factory must be callable"):
            GitForData("not_callable")

        mock_session = Mock(spec=Session)
        git = GitForData(lambda: mock_session)
        assert git._db_factory is not None

    def test_sandbox_requires_session(self):
        with pytest.raises(TypeError, match="db_factory must be callable"):
            Sandbox("not_callable")

        mock_session = Mock(spec=Session)
        sandbox = Sandbox(lambda: mock_session)
        assert sandbox._db_factory is not None

    def test_self_improving_selector_requires_session(self):
        with pytest.raises(TypeError, match="db_factory must be callable"):
            SelfImprovingSelector("not_callable")

        mock_session = Mock(spec=Session)
        selector = SelfImprovingSelector(lambda: mock_session)
        assert selector._db_factory is not None

    def test_pipeline_requires_session(self):
        """SkillPipeline takes a db_factory callable."""
        mock_session = Mock(spec=Session)
        mock_session.query.return_value.filter.return_value.limit.return_value.all.return_value = []
        pipeline = SkillPipeline(lambda: mock_session, llm_client=None)
        assert pipeline._modern._db_factory is not None

    def test_modern_selector_requires_session(self):
        """ModernSkillSelector takes a db_factory callable and validates it."""
        with pytest.raises(TypeError, match="db_factory must be callable"):
            ModernSkillSelector(db_factory="not_callable")

        mock_session = Mock(spec=Session)
        mock_session.query.return_value.filter.return_value.limit.return_value.all.return_value = []
        selector = ModernSkillSelector(db_factory=lambda: mock_session)
        assert selector._db_factory is not None

    def test_semantic_diff_requires_session(self):
        with pytest.raises(TypeError, match="db_factory must be callable"):
            SemanticDiff("not_callable")

        mock_session = Mock(spec=Session)
        diff = SemanticDiff(lambda: mock_session)
        assert diff._db_factory is not None

    def test_skill_manager_requires_session(self):
        """SkillManager inherits DbConsumer — requires callable db_factory."""
        with pytest.raises(TypeError, match="db_factory must be callable"):
            SkillManager("not_callable", CredentialManager("test"))

        mock_session = Mock(spec=Session)
        mgr = SkillManager(lambda: mock_session, CredentialManager("test"))
        assert mgr._db_factory is not None


class TestSessionSharing:
    """Test that DbConsumer modules share factory with children."""

    def test_sandbox_shares_factory_with_branch(self):
        mock_session = Mock(spec=Session)
        factory = lambda: mock_session
        sandbox = Sandbox(factory)
        assert sandbox.branch._db_factory is factory

    def test_pipeline_shares_session(self):
        mock_session = Mock(spec=Session)
        mock_session.query.return_value.filter.return_value.limit.return_value.all.return_value = []
        factory = lambda: mock_session
        pipeline = SkillPipeline(factory, llm_client=None)
        assert pipeline._modern._db_factory is factory

    def test_self_improving_selector_shares_factory(self):
        mock_session = Mock(spec=Session)
        factory = lambda: mock_session
        selector = SelfImprovingSelector(factory)
        assert selector._db_factory is factory


class TestNoSessionCreation:
    """Test that Core modules don't create their own sessions."""

    def test_modules_dont_have_sessionlocal_import(self):
        import inspect
        for cls in [GitForData, EventLogger, Sandbox, SkillRegistry, SkillPipeline,
                    ModernSkillSelector, SelfImprovingSelector, ToolMockingLayer, SemanticDiff,
                    SkillManager]:
            source = inspect.getsource(cls.__init__)
            assert "SessionLocal()" not in source, f"{cls.__name__} creates SessionLocal"

    def test_modules_dont_have_lazy_session(self):
        mock_session = Mock(spec=Session)
        mock_session.query.return_value.filter.return_value.all.return_value = []
        mock_session.query.return_value.filter.return_value.limit.return_value.all.return_value = []
        modules = [
            GitForData(lambda: mock_session),
            EventLogger.from_session(mock_session),
            Sandbox(lambda: mock_session),
            SkillRegistry(db_factory=lambda: mock_session),
            ModernSkillSelector(db_factory=lambda: mock_session),
            ToolMockingLayer(MockMode.PRODUCTION, db_factory=lambda: mock_session),
            SemanticDiff(lambda: mock_session),
            SkillManager(lambda: mock_session, CredentialManager("test")),
        ]
        for module in modules:
            assert not hasattr(module, '_lazy_session'), f"{type(module).__name__} has _lazy_session"
            assert not hasattr(module, '_owns_session'), f"{type(module).__name__} has _owns_session"


class TestSessionLifecycle:
    """Test that DbConsumer._db() manages session lifecycle."""

    def test_db_context_manager_closes_session(self):
        """_db() must close the session when the block exits."""
        mock_session = Mock(spec=Session)
        git = GitForData(lambda: mock_session)

        with git._db() as db:
            assert db is mock_session
        mock_session.close.assert_called_once()

    def test_db_context_manager_rolls_back_on_exception(self):
        """_db() must rollback before closing when an exception occurs."""
        mock_session = Mock(spec=Session)
        git = GitForData(lambda: mock_session)

        with pytest.raises(ValueError):
            with git._db() as db:
                raise ValueError("boom")
        mock_session.rollback.assert_called_once()
        mock_session.close.assert_called_once()

    def test_event_logger_borrowed_mode_does_not_close(self):
        """EventLogger.from_session uses a borrowed session — no close on use."""
        mock_session = Mock(spec=Session)
        logger = EventLogger.from_session(mock_session)
        # Borrowed mode: session is not closed by EventLogger
        mock_session.close.assert_not_called()

    def test_no_session_ownership_attributes(self):
        """DbConsumer modules must not have legacy session ownership attrs."""
        mock_session = Mock(spec=Session)
        mock_session.query.return_value.filter.return_value.all.return_value = []
        modules = [
            GitForData(lambda: mock_session),
            Sandbox(lambda: mock_session),
            SelfImprovingSelector(lambda: mock_session),
            SemanticDiff(lambda: mock_session),
            SkillManager(lambda: mock_session, CredentialManager("test")),
        ]
        for module in modules:
            assert not hasattr(module, '_lazy_session'), f"{type(module).__name__} has _lazy_session"
            assert not hasattr(module, '_owns_session'), f"{type(module).__name__} has _owns_session"
            assert not hasattr(module, 'db'), f"{type(module).__name__} has legacy .db attribute"
