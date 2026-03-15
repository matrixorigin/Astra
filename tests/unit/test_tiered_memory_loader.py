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
        """Test loading semantic memories via search."""
        from core.context.tiered_loader import TieredMemoryLoader
        
        with patch('core.memory.backends.memoria_http.MemoriaHTTPClient') as mock_client_class:
            mock_client = Mock()
            mock_client.search.return_value = [
                {'content': 'Use pytest -n auto for parallel tests', 'memory_type': 'semantic'},
                {'content': 'Run make test for CI', 'memory_type': 'procedural'}
            ]
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
            mock_client.search.assert_called_once()

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
            mock_client.search.return_value = [
                {'content': 'Semantic memory', 'memory_type': 'semantic'}
            ]
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
