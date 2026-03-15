"""Regression tests for Bug 21: create_editor/get_memoria_storage with empty user_id."""

from __future__ import annotations
import os
import pytest
from unittest.mock import patch


class TestEmptyUserIDGuard:
    def test_create_editor_raises_on_empty_user_id(self):
        """Bug 21: create_editor must raise ValueError, not silently use 'default'."""
        with patch.dict(os.environ, {
            "MEMORIA_BASE_URL": "http://localhost:8100",
            "MEMORIA_MASTER_KEY": "k",
        }):
            from core.memory.factory import create_editor
            with pytest.raises(ValueError, match="user_id"):
                create_editor(None, user_id=None)

    def test_create_editor_raises_on_empty_string_user_id(self):
        with patch.dict(os.environ, {
            "MEMORIA_BASE_URL": "http://localhost:8100",
            "MEMORIA_MASTER_KEY": "k",
        }):
            from core.memory.factory import create_editor
            with pytest.raises(ValueError, match="user_id"):
                create_editor(None, user_id="")

    def test_get_memoria_storage_raises_on_empty_user_id(self):
        """get_memoria_storage must raise ValueError on empty user_id."""
        with patch.dict(os.environ, {
            "MEMORIA_BASE_URL": "http://localhost:8100",
            "MEMORIA_MASTER_KEY": "k",
        }):
            from core.memory.backends import get_memoria_storage
            with pytest.raises(ValueError, match="user_id"):
                get_memoria_storage("")

    def test_get_memoria_storage_valid_user_id(self):
        """Valid user_id must work without error."""
        with patch.dict(os.environ, {
            "MEMORIA_BASE_URL": "http://localhost:8100",
            "MEMORIA_MASTER_KEY": "k",
        }):
            from core.memory.backends import get_memoria_storage
            svc = get_memoria_storage("alice")
            assert svc.user_id == "alice"
