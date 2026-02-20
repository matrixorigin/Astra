"""Unit tests for SessionService sandbox cleanup on close."""

from unittest.mock import Mock, patch, MagicMock
import pytest
from api.services.session_service import SessionService


@pytest.fixture
def service():
    db = Mock()
    with patch("api.services.session_service.SessionRepository"), \
         patch("api.services.session_service.AuditLogger"), \
         patch("api.services.session_service.PermissionChecker"):
        return SessionService(db)


class TestCleanupSandbox:
    def test_cleanup_deletes_active_sandboxes(self, service):
        """status=closed triggers sandbox delete for matching session."""
        row = Mock()
        row._mapping = {"sandbox_name": "sandbox_abc"}
        service.db_session.execute.return_value = [row]

        with patch("core.sandbox.Sandbox") as MockSandbox:
            mock_sb = MockSandbox.return_value
            service._cleanup_sandbox("sess-1")
            mock_sb.delete.assert_called_once_with("sandbox_abc", force=True)

    def test_cleanup_no_sandboxes(self, service):
        """No-op when session has no sandboxes."""
        service.db_session.execute.return_value = []
        # Should not raise
        service._cleanup_sandbox("sess-2")

    def test_cleanup_swallows_errors(self, service):
        """Cleanup is best-effort — exceptions are swallowed."""
        service.db_session.execute.side_effect = RuntimeError("db down")
        # Should not raise
        service._cleanup_sandbox("sess-3")

    def _make_session_mock(self, session_id="s1", user_id="u1", status="active"):
        s = Mock()
        s.session_id = session_id
        s.user_id = user_id
        s.agent_id = "a1"
        s.title = "t"
        s.session_metadata = {}
        s.status = status
        s.event_count = 0
        s.created_at = Mock(isoformat=Mock(return_value=""))
        s.updated_at = Mock(isoformat=Mock(return_value=""))
        s.ended_at = None
        return s

    def test_update_session_triggers_cleanup_on_closed(self, service):
        """update_session with status=closed calls _cleanup_sandbox."""
        session_mock = self._make_session_mock()
        service.session_repo.get_by_id = Mock(return_value=session_mock)
        updated = self._make_session_mock(status="closed")
        service.session_repo.update = Mock(return_value=updated)

        with patch.object(service, "_cleanup_sandbox") as mock_clean:
            service.update_session("s1", "u1", status="closed")
            mock_clean.assert_called_once_with("s1")

    def test_update_session_no_cleanup_on_active(self, service):
        """update_session with status=active does NOT call _cleanup_sandbox."""
        session_mock = self._make_session_mock()
        service.session_repo.get_by_id = Mock(return_value=session_mock)
        updated = self._make_session_mock()
        service.session_repo.update = Mock(return_value=updated)

        with patch.object(service, "_cleanup_sandbox") as mock_clean:
            service.update_session("s1", "u1", status="active")
            mock_clean.assert_not_called()
