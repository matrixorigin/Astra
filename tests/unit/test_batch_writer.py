"""Unit tests for core/events/batch_writer.py."""

import time
from unittest.mock import MagicMock, patch, call

import pytest

from core.events.batch_writer import BatchEventWriter, get_batch_writer


@pytest.fixture(autouse=True)
def reset_global_writer():
    import core.events.batch_writer as mod

    mod._writer = None
    yield
    mod._writer = None


def make_writer(batch_size=10, flush_interval=0.1):
    mock_db = MagicMock()
    mock_session = MagicMock()
    mock_db.return_value.__enter__ = MagicMock(return_value=mock_session)
    mock_db.return_value.__exit__ = MagicMock(return_value=False)
    writer = BatchEventWriter(mock_db, batch_size=batch_size, flush_interval=flush_interval)
    return writer, mock_db, mock_session


class TestBatchEventWriterLifecycle:
    def test_start_and_stop(self):
        writer, _, _ = make_writer()
        writer.start()
        assert writer._running is True
        assert writer._thread is not None
        writer.stop()
        assert writer._running is False

    def test_start_idempotent(self):
        writer, _, _ = make_writer()
        writer.start()
        thread1 = writer._thread
        writer.start()
        assert writer._thread is thread1
        writer.stop()

    def test_write_event_queues(self):
        writer, _, _ = make_writer()
        writer.write_event({"event_id": "e1"})
        assert writer._queue.qsize() == 1


class TestBatchEventWriterFlush:
    def test_flush_empty_buffer_noop(self):
        writer, mock_db, mock_session = make_writer()
        writer._flush()
        mock_session.execute.assert_not_called()

    def test_flush_calls_batch_insert(self):
        writer, mock_db, mock_session = make_writer()
        writer._buffer = [
            {
                "event_id": "e1",
                "user_id": "u",
                "session_id": "s",
                "agent_id": None,
                "agent_version": None,
                "event_type": "user_query",
                "content": "hi",
                "parent_event_id": None,
                "causal_chain_id": None,
                "created_at": None,
                "metadata": None,
            }
        ]
        with patch.object(writer, "_batch_insert") as mock_insert:
            writer._flush()
        mock_insert.assert_called_once()
        assert writer._buffer == []

    def test_flush_requeues_on_error(self):
        writer, _, _ = make_writer()
        event = {"event_id": "e1"}
        writer._buffer = [event]
        with patch.object(writer, "_batch_insert", side_effect=Exception("db error")):
            writer._flush()
        assert writer._queue.qsize() == 1


class TestGetBatchWriter:
    def test_returns_none_without_factory(self):
        assert get_batch_writer() is None

    def test_creates_and_returns_writer(self):
        mock_factory = MagicMock()
        with patch.object(BatchEventWriter, "start"):
            w = get_batch_writer(mock_factory)
        assert isinstance(w, BatchEventWriter)

    def test_singleton(self):
        mock_factory = MagicMock()
        with patch.object(BatchEventWriter, "start"):
            w1 = get_batch_writer(mock_factory)
            w2 = get_batch_writer(mock_factory)
        assert w1 is w2
