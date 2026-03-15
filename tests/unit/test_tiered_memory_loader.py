"""Test TieredMemoryLoader with Memoria backend."""

import os
import pytest
from unittest.mock import Mock, patch


class TestTieredMemoryLoaderMemoria:
    """Test TieredMemoryLoader integration with Memoria HTTP client."""

    def test_load_l0_with_memoria_client(self):
        """Test loading profile memories from Memoria."""
        from core.context.tiered_loader import TieredMemoryLoader
        
        with patch('core.memory.backends.memoria_http.MemoriaHTTPClient') as mock_client_class:
            mock_client = Mock()
            mock_client.list_memories.return_value = {
                'items': [
                    {'content': 'Python测试需要-n auto参数', 'memory_type': 'profile'},
                    {'content': 'User prefers vim', 'memory_type': 'profile'}
                ]
            }
            mock_client_class.return_value = mock_client
            
            # Set env vars to trigger Memoria client creation
            with patch.dict(os.environ, {
                'MEMORIA_BASE_URL': 'http://localhost:8100',
                'MEMORIA_MASTER_KEY': 'test-key'
            }):
                loader = TieredMemoryLoader()
                result = loader.load_l0('testuser')
            
            assert 'Python测试需要-n auto参数' in result
            assert 'User prefers vim' in result
            mock_client.list_memories.assert_called_once_with(
                user_id='testuser',
                memory_type='profile',
                limit=10
            )

    def test_load_l0_empty_memories(self):
        """Test loading when no memories exist."""
        from core.context.tiered_loader import TieredMemoryLoader
        
        with patch('core.memory.backends.memoria_http.MemoriaHTTPClient') as mock_client_class:
            mock_client = Mock()
            mock_client.list_memories.return_value = {'items': []}
            mock_client_class.return_value = mock_client
            
            with patch.dict(os.environ, {
                'MEMORIA_BASE_URL': 'http://localhost:8100',
                'MEMORIA_MASTER_KEY': 'test-key'
            }):
                loader = TieredMemoryLoader()
                result = loader.load_l0('testuser')
            
            assert result == ""

    def test_load_l1_with_memoria_search(self):
        """Test loading semantic memories via retrieve (includes episodic)."""
        from core.context.tiered_loader import TieredMemoryLoader
        
        with patch('core.memory.backends.memoria_http.MemoriaHTTPClient') as mock_client_class:
            mock_client = Mock()
            mock_client.retrieve.return_value = {
                "results": [
                    {'content': 'Use pytest -n auto for parallel tests', 'memory_type': 'semantic'},
                    {'content': 'Run make test for CI', 'memory_type': 'procedural'},
                ]
            }
            mock_client_class.return_value = mock_client
            
            with patch.dict(os.environ, {
                'MEMORIA_BASE_URL': 'http://localhost:8100',
                'MEMORIA_MASTER_KEY': 'test-key'
            }):
                loader = TieredMemoryLoader()
                result, stats = loader.load_l1('testuser', 'session-123', 'python testing')
            
            assert 'Relevant Memories:' in result
            assert 'pytest -n auto' in result
            assert 'make test' in result
            mock_client.retrieve.assert_called_once()
            # Verify episodic is included in memory_types
            call_kwargs = mock_client.retrieve.call_args
            assert 'episodic' in call_kwargs.kwargs.get('memory_types', [])

    def test_load_l0_handles_exceptions(self):
        """Test that L0 loading handles exceptions gracefully."""
        from core.context.tiered_loader import TieredMemoryLoader
        
        with patch('core.memory.backends.memoria_http.MemoriaHTTPClient') as mock_client_class:
            mock_client = Mock()
            mock_client.list_memories.side_effect = Exception("Connection failed")
            mock_client_class.return_value = mock_client
            
            with patch.dict(os.environ, {
                'MEMORIA_BASE_URL': 'http://localhost:8100',
                'MEMORIA_MASTER_KEY': 'test-key'
            }):
                loader = TieredMemoryLoader()
                result = loader.load_l0('testuser')
            
            # Should return empty string on error, not raise
            assert result == ""

    def test_build_section_combines_l0_and_l1(self):
        """Test that build_section combines profile and semantic memories."""
        from core.context.tiered_loader import TieredMemoryLoader
        
        with patch('core.memory.backends.memoria_http.MemoriaHTTPClient') as mock_client_class:
            mock_client = Mock()
            mock_client.list_memories.return_value = {
                'items': [{'content': 'Profile memory', 'memory_type': 'profile'}]
            }
            mock_client.retrieve.return_value = {
                "results": [{'content': 'Semantic memory', 'memory_type': 'semantic'}]
            }
            mock_client_class.return_value = mock_client
            
            with patch.dict(os.environ, {
                'MEMORIA_BASE_URL': 'http://localhost:8100',
                'MEMORIA_MASTER_KEY': 'test-key'
            }):
                loader = TieredMemoryLoader()
                section, stats = loader.build_section(
                    user_id='testuser',
                    session_id='session-123',
                    query='test query'
                )
            
            assert 'Profile memory' in section
            assert 'Semantic memory' in section
            # stats may be None when using Memoria client (no detailed stats)
            # Just verify the section was built correctly

    def test_no_memoria_config_returns_empty(self):
        """Test that loader returns empty when Memoria is not configured."""
        from core.context.tiered_loader import TieredMemoryLoader
        
        # No MEMORIA_BASE_URL in env
        with patch.dict(os.environ, {}, clear=True):
            loader = TieredMemoryLoader()
            result = loader.load_l0('testuser')
            
            assert result == ""


class TestTieredLoaderL0Profile:
    """load_l0 must prefer synthesized profile over raw memory list."""

    def test_load_l0_uses_get_profile_first(self):
        """get_profile returns synthesized profile — must be used over list_memories."""
        from core.context.tiered_loader import TieredMemoryLoader

        with patch('core.memory.backends.memoria_http.MemoriaHTTPClient') as mock_cls:
            mock_client = Mock()
            mock_client.get_profile.return_value = {
                "profile": "User is a Python developer who prefers pytest.",
                "stats": {}
            }
            mock_cls.return_value = mock_client

            with patch.dict(os.environ, {
                'MEMORIA_BASE_URL': 'http://localhost:8100',
                'MEMORIA_MASTER_KEY': 'test-key',
            }):
                loader = TieredMemoryLoader()
                result = loader.load_l0('user1')

        assert "Python developer" in result
        mock_client.get_profile.assert_called_once()
        mock_client.list_memories.assert_not_called()

    def test_load_l0_falls_back_to_list_when_profile_empty(self):
        """If get_profile returns no profile, fall back to list_memories."""
        from core.context.tiered_loader import TieredMemoryLoader

        with patch('core.memory.backends.memoria_http.MemoriaHTTPClient') as mock_cls:
            mock_client = Mock()
            mock_client.get_profile.return_value = {"profile": None, "stats": {}}
            mock_client.list_memories.return_value = {
                "items": [{"content": "User prefers dark mode", "memory_type": "profile"}]
            }
            mock_cls.return_value = mock_client

            with patch.dict(os.environ, {
                'MEMORIA_BASE_URL': 'http://localhost:8100',
                'MEMORIA_MASTER_KEY': 'test-key',
            }):
                loader = TieredMemoryLoader()
                result = loader.load_l0('user1')

        assert "dark mode" in result
        mock_client.list_memories.assert_called_once()


class TestTieredLoaderRegressions:
    """Regression tests for bugs found during Memoria integration."""

    def test_load_l1_uses_retrieve_not_search(self):
        """Regression: load_l1 must call retrieve(), not search() (Bug 2).

        search() has no memory_types filter — episodic memories would be excluded.
        retrieve() supports memory_types=[semantic, procedural, episodic].
        """
        from core.context.tiered_loader import TieredMemoryLoader

        with patch('core.memory.backends.memoria_http.MemoriaHTTPClient') as mock_cls:
            mock_client = Mock()
            mock_client.retrieve.return_value = {"results": []}
            mock_cls.return_value = mock_client

            with patch.dict(os.environ, {
                'MEMORIA_BASE_URL': 'http://localhost:8100',
                'MEMORIA_MASTER_KEY': 'test-key',
            }):
                loader = TieredMemoryLoader()
                loader.load_l1('user1', 'sess1', 'query')

            mock_client.retrieve.assert_called_once()
            mock_client.search.assert_not_called()

    def test_load_l1_includes_episodic_in_memory_types(self):
        """Regression: retrieve() must include 'episodic' in memory_types (Bug 1+2).

        Without episodic, cross-session activity summaries are never retrieved.
        """
        from core.context.tiered_loader import TieredMemoryLoader

        with patch('core.memory.backends.memoria_http.MemoriaHTTPClient') as mock_cls:
            mock_client = Mock()
            mock_client.retrieve.return_value = {"results": []}
            mock_cls.return_value = mock_client

            with patch.dict(os.environ, {
                'MEMORIA_BASE_URL': 'http://localhost:8100',
                'MEMORIA_MASTER_KEY': 'test-key',
            }):
                loader = TieredMemoryLoader()
                loader.load_l1('user1', 'sess1', 'query')

            call_kwargs = mock_client.retrieve.call_args.kwargs
            assert 'episodic' in call_kwargs['memory_types'], (
                "episodic must be in memory_types — cross-session summaries depend on it"
            )
            assert 'semantic' in call_kwargs['memory_types']
            assert 'procedural' in call_kwargs['memory_types']

    def test_load_l1_retrieve_results_key(self):
        """Regression: retrieve() returns {'results': [...]}, not {'memories': [...]}.

        Wrong key → empty L1 section even when memories exist.
        """
        from core.context.tiered_loader import TieredMemoryLoader

        with patch('core.memory.backends.memoria_http.MemoriaHTTPClient') as mock_cls:
            mock_client = Mock()
            mock_client.retrieve.return_value = {
                "results": [
                    {"content": "Session Summary: G14 fix completed", "memory_type": "episodic"},
                ]
            }
            mock_cls.return_value = mock_client

            with patch.dict(os.environ, {
                'MEMORIA_BASE_URL': 'http://localhost:8100',
                'MEMORIA_MASTER_KEY': 'test-key',
            }):
                loader = TieredMemoryLoader()
                result, _ = loader.load_l1('user1', 'sess1', 'Memoria fix')

            assert 'G14 fix completed' in result, (
                "Memory content must appear — 'results' key must be read correctly"
            )
            assert '[episodic]' in result


class TestInvalidateProfile:
    """Bug 14: invalidate_profile crashes when _svc is None (Memoria mode)."""

    def test_invalidate_profile_no_crash_when_svc_is_none(self):
        """Regression: invalidate_profile must not raise when _svc is None."""
        from core.context.tiered_loader import TieredMemoryLoader

        loader = TieredMemoryLoader(memory_service=None)
        loader._memoria_client = None  # no client either
        # Must not raise AttributeError
        loader.invalidate_profile("user1")

    def test_invalidate_profile_calls_svc_when_present(self):
        """When _svc is set, invalidate_profile must delegate to it."""
        from core.context.tiered_loader import TieredMemoryLoader
        from unittest.mock import Mock

        mock_svc = Mock()
        loader = TieredMemoryLoader(memory_service=mock_svc)
        loader.invalidate_profile("user1")
        mock_svc.invalidate_profile.assert_called_once_with("user1")
