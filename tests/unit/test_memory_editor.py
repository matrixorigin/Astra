"""Tests for MemoryEditor — covers 0% coverage gap and Bug 7 regressions.

Bug 7: MemoryEditor.correct() called storage.correct(memory_id, content, reason)
       but MemoriaStorage.correct() signature is correct(user_id, memory_id, content, reason).
       Also: purge() called non-existent purge_by_id/purge_by_topic methods.
"""

from __future__ import annotations

import pytest
from unittest.mock import MagicMock, patch


def _make_editor(user_id="user1"):
    from core.memory.editor import MemoryEditor
    storage = MagicMock()
    storage.user_id = user_id
    return MemoryEditor(storage), storage


class TestMemoryEditorInject:
    def test_inject_calls_storage_store(self):
        editor, storage = _make_editor()
        from core.memory.types import MemoryType, Memory
        storage.store.return_value = MagicMock(spec=Memory)

        editor.inject("hello world", memory_type="semantic")

        storage.store.assert_called_once()
        call_kwargs = storage.store.call_args
        assert call_kwargs.args[0] == "user1"
        assert call_kwargs.args[1] == "hello world"

    def test_inject_passes_session_id(self):
        editor, storage = _make_editor()
        from core.memory.types import Memory
        storage.store.return_value = MagicMock(spec=Memory)

        editor.inject("test", memory_type="semantic", session_id="sess-abc")

        call_kwargs = storage.store.call_args.kwargs
        assert call_kwargs.get("session_id") == "sess-abc"


class TestMemoryEditorCorrect:
    def test_correct_passes_user_id(self):
        """Bug 7 regression: correct must pass user_id as first arg to storage.correct."""
        editor, storage = _make_editor(user_id="alice")
        from core.memory.types import Memory
        storage.correct.return_value = MagicMock(spec=Memory)

        editor.correct("mem-123", "new content", reason="updated")

        storage.correct.assert_called_once_with(
            "alice", "mem-123", "new content", reason="updated"
        )

    def test_correct_without_reason(self):
        editor, storage = _make_editor()
        from core.memory.types import Memory
        storage.correct.return_value = MagicMock(spec=Memory)

        editor.correct("mem-123", "new content")

        call_kwargs = storage.correct.call_args
        assert call_kwargs.kwargs.get("reason") == ""


class TestMemoryEditorPurge:
    def test_purge_by_id_calls_storage_purge_with_list(self):
        """Bug 7 regression: purge must call storage.purge (not purge_by_id)."""
        editor, storage = _make_editor(user_id="alice")
        storage.purge.return_value = MagicMock(deactivated=1)

        editor.purge(memory_id="mem-123", reason="done")

        storage.purge.assert_called_once_with(
            "alice", memory_ids=["mem-123"], reason="done"
        )

    def test_purge_by_topic_calls_storage_purge_with_topic(self):
        """Bug 7 regression: purge by topic must call storage.purge with topic kwarg."""
        editor, storage = _make_editor(user_id="alice")
        storage.purge.return_value = MagicMock(deactivated=3)

        editor.purge(topic="old project", reason="cleanup")

        storage.purge.assert_called_once_with(
            "alice", topic="old project", reason="cleanup"
        )

    def test_purge_requires_id_or_topic(self):
        editor, storage = _make_editor()

        with pytest.raises(ValueError, match="Either memory_id or topic"):
            editor.purge()


class TestMemoryEditorRetrieve:
    def test_retrieve_passes_user_id(self):
        """Bug 8 regression: retrieve must pass user_id to storage.retrieve."""
        editor, storage = _make_editor(user_id="bob")
        storage.retrieve.return_value = ([], None)

        editor.retrieve("what is Python?", top_k=3)

        storage.retrieve.assert_called_once()
        args = storage.retrieve.call_args.args
        assert args[0] == "bob"
        assert args[1] == "what is Python?"
        assert storage.retrieve.call_args.kwargs.get("top_k") == 3

    def test_retrieve_returns_memory_list(self):
        from core.memory.types import Memory, MemoryType
        editor, storage = _make_editor()
        m = MagicMock(spec=Memory)
        storage.retrieve.return_value = ([m], None)

        result = editor.retrieve("query")

        assert result == [m]


class TestMemoryEditorBatchInject:
    def test_batch_inject_dict_memories(self):
        editor, storage = _make_editor(user_id="user1")
        storage.client = MagicMock()
        storage.client.batch_store.return_value = [{"memory_id": "m1"}, {"memory_id": "m2"}]

        editor.batch_inject("user1", [
            {"content": "fact 1", "memory_type": "semantic"},
            {"content": "fact 2", "memory_type": "procedural"},
        ])

        # Must use batch API — single call, not N individual store calls
        storage.client.batch_store.assert_called_once()
        storage.store.assert_not_called()

    def test_batch_inject_string_memories(self):
        editor, storage = _make_editor(user_id="user1")
        storage.client = MagicMock()
        storage.client.batch_store.return_value = [{"memory_id": "m1"}, {"memory_id": "m2"}]

        editor.batch_inject("user1", ["fact 1", "fact 2"])

        storage.client.batch_store.assert_called_once()
        storage.store.assert_not_called()

    def test_batch_inject_wrong_user_id_raises(self):
        """Bug 9 regression: batch_inject must reject mismatched user_id."""
        editor, storage = _make_editor(user_id="alice")

        with pytest.raises(ValueError, match="does not match editor user_id"):
            editor.batch_inject("bob", [{"content": "test", "memory_type": "semantic"}])

        storage.store.assert_not_called()

    def test_batch_inject_correct_user_id_succeeds(self):
        editor, storage = _make_editor(user_id="alice")
        storage.client = MagicMock()
        storage.client.batch_store.return_value = [{"memory_id": "m1"}]

        editor.batch_inject("alice", [{"content": "test", "memory_type": "semantic"}])

        storage.client.batch_store.assert_called_once()
    def test_purge_passes_topic_to_client(self):
        """MemoriaStorage.purge must forward topic kwarg to client.purge."""
        from core.memory.backends.memoria_http import MemoriaStorage

        storage = MemoriaStorage.__new__(MemoriaStorage)
        storage.client = MagicMock()
        storage.client.purge.return_value = {"purged": 2}

        storage.purge("user1", topic="old stuff", reason="cleanup")

        call_kwargs = storage.client.purge.call_args.kwargs
        assert call_kwargs.get("topic") == "old stuff"
        assert call_kwargs.get("reason") == "cleanup"
