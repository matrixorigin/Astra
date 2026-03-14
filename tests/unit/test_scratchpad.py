"""Tests for agent scratchpad."""

import pytest
from unittest.mock import Mock, patch
from core.context.scratchpad import AgentScratchpad


class TestAgentScratchpad:
    """Test agent scratchpad for working memory."""

    @pytest.fixture
    def mock_db(self):
        """Mock database session."""
        return Mock()

    @pytest.fixture
    def scratchpad(self, mock_db):
        """Create scratchpad."""
        return AgentScratchpad(lambda: mock_db)

    def test_create_note(self, scratchpad, mock_db):
        """Test note creation."""
        note_id = scratchpad.create_note(
            session_id="sess_123",
            user_id="alice",
            note_type="plan",
            content="Test plan",
        )

        assert note_id.startswith("note_")
        mock_db.add.assert_called_once()
        mock_db.commit.assert_called_once()

    def test_get_active_notes(self, scratchpad, mock_db):
        """Test getting active notes."""
        # Mock query result
        mock_note = Mock()
        mock_note.note_id = "note_123"
        mock_note.note_type = "plan"
        mock_note.content = "Test plan"
        mock_note.created_at = None
        mock_note.related_event_ids = []

        # Fix mock chain
        mock_query = Mock()
        mock_query.filter.return_value = mock_query
        mock_query.order_by.return_value = mock_query
        mock_query.all.return_value = [mock_note]
        mock_db.query.return_value = mock_query

        notes = scratchpad.get_active_notes("sess_123")

        assert len(notes) == 1
        assert notes[0]["note_id"] == "note_123"
        assert notes[0]["note_type"] == "plan"

    def test_get_cross_session_notes(self, scratchpad, mock_db):
        """Test getting notes across sessions."""
        # Mock query result
        mock_note = Mock()
        mock_note.note_id = "note_123"
        mock_note.session_id = "sess_old"
        mock_note.note_type = "todo"
        mock_note.content = "Unfinished task"
        mock_note.created_at = None
        mock_note.updated_at = None

        # Fix mock chain
        mock_query = Mock()
        mock_query.filter.return_value = mock_query
        mock_query.order_by.return_value = mock_query
        mock_query.limit.return_value = mock_query
        mock_query.all.return_value = [mock_note]
        mock_db.query.return_value = mock_query

        notes = scratchpad.get_cross_session_notes("alice")

        assert len(notes) == 1
        assert notes[0]["session_id"] == "sess_old"

    def test_update_note(self, scratchpad, mock_db):
        """Test note update."""
        # Mock existing note
        mock_note = Mock()
        mock_note.content = "Old content"

        mock_db.query.return_value.filter.return_value.first.return_value = mock_note

        result = scratchpad.update_note("note_123", "New content")

        assert result is True
        assert mock_note.content == "New content"
        mock_db.commit.assert_called_once()

    def test_update_note_append(self, scratchpad, mock_db):
        """Test note update with append."""
        # Mock existing note
        mock_note = Mock()
        mock_note.content = "Old content"

        mock_db.query.return_value.filter.return_value.first.return_value = mock_note

        result = scratchpad.update_note("note_123", "New content", append=True)

        assert result is True
        assert "Old content" in mock_note.content
        assert "New content" in mock_note.content

    def test_close_note(self, scratchpad, mock_db):
        """Test note closure."""
        # Mock existing note
        mock_note = Mock()
        mock_note.status = "active"

        mock_db.query.return_value.filter.return_value.first.return_value = mock_note

        result = scratchpad.close_note("note_123", status="completed")

        assert result is True
        assert mock_note.status == "completed"
        mock_db.commit.assert_called_once()

    def test_link_notes(self, scratchpad, mock_db):
        """Test note linking."""
        # Mock existing note
        mock_note = Mock()
        mock_note.related_note_ids = []

        mock_db.query.return_value.filter.return_value.first.return_value = mock_note

        result = scratchpad.link_notes("note_123", ["note_456", "note_789"])

        assert result is True
        assert mock_note.related_note_ids == ["note_456", "note_789"]
        mock_db.commit.assert_called_once()

    def test_create_note_invalid_type(self, scratchpad, mock_db):
        """Test note creation with invalid type."""
        with pytest.raises(ValueError, match="Invalid note_type"):
            scratchpad.create_note(
                session_id="sess_123",
                user_id="alice",
                note_type="invalid_type",
                content="Test",
            )
