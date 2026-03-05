"""Tests for review issues A–G (Skills/Context/Chat module fixes).

Covers:
  A+B – _build_chat_loop caches SkillCatalog + register_builtin_skills (singleton)
  C   – ContextManager._get_skill_definitions 60s cache
  E   – _retrieve_relevant_context reuses shared HybridRetriever/EmbeddingService
  F   – Phase 5 uses atomic increment, not COUNT(*)
  G   – _gather_history uses MATCH AGAINST, not LIKE
"""

import inspect
import threading

import pytest
from sqlalchemy import text
from uuid_utils import uuid7


# ── A+B: Shared components singleton ─────────────────────────────────

class TestSharedComponentsSingleton:
    """_build_chat_loop should reuse shared SkillCatalog across calls."""

    def test_shared_components_initialized_once(self):
        """_get_shared_components is idempotent — second call is a no-op."""
        import os
        os.environ['DISABLE_GATE_TRIGGER'] = '1'
        from api.routers.chat import (
            _get_shared_components, _shared_skill_catalog,
            _shared_tool_registry, _shared_context_manager,
        )
        from api.database import SessionLocal

        _get_shared_components(SessionLocal)
        # Capture references
        from api.routers import chat as _mod
        cat1 = _mod._shared_skill_catalog
        reg1 = _mod._shared_tool_registry
        ctx1 = _mod._shared_context_manager

        # Call again — should be same objects
        _get_shared_components(SessionLocal)
        assert _mod._shared_skill_catalog is cat1
        assert _mod._shared_tool_registry is reg1
        assert _mod._shared_context_manager is ctx1

    def test_build_chat_loop_source_no_register_builtin(self):
        """_build_chat_loop body should NOT call register_builtin_skills directly."""
        from api.routers.chat import _build_chat_loop
        src = inspect.getsource(_build_chat_loop)
        assert "register_builtin_skills" not in src, \
            "_build_chat_loop should delegate to _get_shared_components"


# ── C: _get_skill_definitions cache ──────────────────────────────────

class TestSkillDefinitionsCache:
    def test_second_call_uses_cache(self):
        """Two rapid calls should hit cache (no second DB query)."""
        from unittest.mock import MagicMock, patch
        from core.context.manager import ContextManager

        mgr = ContextManager.__new__(ContextManager)
        mgr._db_factory = MagicMock()
        mgr._skill_def_cache = None

        mock_db = MagicMock()
        mock_skill = MagicMock()
        mock_skill.skill_name = "test"
        mock_skill.description = "desc"
        mock_skill.version = "1.0"
        mock_skill.skill_definition = None
        mock_skill.triggers = None
        mock_db.query.return_value.filter.return_value.all.return_value = [mock_skill]

        mgr._db = MagicMock(return_value=MagicMock(
            __enter__=MagicMock(return_value=mock_db),
            __exit__=MagicMock(return_value=False),
        ))

        r1 = mgr._get_skill_definitions(10000)
        r2 = mgr._get_skill_definitions(10000)
        assert r1 == r2
        # DB queried only once
        assert mock_db.query.call_count == 1


# ── F: Phase 5 atomic increment ─────────────────────────────────────

class TestPhase5AtomicIncrement:
    def test_no_count_star_in_phase5(self):
        """Phase 5 should use event_count + :n, not SELECT COUNT(*)."""
        from api.routers import chat as _mod
        src = inspect.getsource(_mod)
        # Find Phase 5 section
        idx = src.find("Phase 5:")
        assert idx > 0
        phase5_block = src[idx:idx + 1000]
        assert "COUNT(*)" not in phase5_block
        assert "event_count + :n" in phase5_block


# ── G: _gather_history fulltext ──────────────────────────────────────

class TestGatherHistoryFulltext:
    def test_uses_match_against_not_like(self):
        """_gather_history should use MATCH AGAINST, not LIKE."""
        from api.routers.chat import _gather_history
        src = inspect.getsource(_gather_history)
        assert "MATCH" in src and "AGAINST" in src, "Should use fulltext MATCH AGAINST"
        assert ".like(" not in src, "Should not use ORM .like() for keyword search"


# ── E: Shared retriever/embed svc ───────────────────────────────────

class TestSharedRetrieverSingleton:
    def test_retriever_reused(self):
        """_get_shared_retriever returns same instance on repeated calls."""
        import os
        os.environ.setdefault('TOKEN_ENCRYPTION_KEY', 't')
        os.environ.setdefault('JWT_SECRET_KEY', 't')
        from api.routers.chat import _get_shared_retriever
        r1 = _get_shared_retriever()
        r2 = _get_shared_retriever()
        assert r1 is r2

    def test_embed_svc_reused(self):
        from api.routers.chat import _get_shared_embed_svc
        s1 = _get_shared_embed_svc()
        s2 = _get_shared_embed_svc()
        assert s1 is s2
