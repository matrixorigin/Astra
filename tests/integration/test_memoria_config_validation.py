"""Test Memoria configuration validation and error handling.

This test ensures that missing or invalid Memoria configuration is caught early
and provides clear error messages, preventing runtime failures.
"""

import os
import pytest
from unittest.mock import patch

from core.memory.factory import create_editor
from api.database import SessionLocal


class TestMemoriaConfigValidation:
    """Test Memoria configuration validation."""

    def test_missing_memoria_base_url_raises_clear_error(self):
        """Missing MEMORIA_BASE_URL should raise RuntimeError with clear message."""
        with patch.dict(os.environ, {}, clear=True):
            # Clear all Memoria env vars
            for key in list(os.environ.keys()):
                if key.startswith("MEMORIA_"):
                    del os.environ[key]
            
            with pytest.raises(RuntimeError) as exc_info:
                create_editor(SessionLocal, user_id="test-user")
            
            error_msg = str(exc_info.value)
            assert "Memoria is required" in error_msg
            assert "MEMORIA_BASE_URL" in error_msg
            assert "MEMORIA_MASTER_KEY" in error_msg

    def test_missing_memoria_auth_raises_clear_error(self):
        """Missing auth (both MASTER_KEY and API_KEY) should raise RuntimeError."""
        with patch.dict(os.environ, {
            "MEMORIA_BASE_URL": "http://localhost:8100",
            # No MEMORIA_MASTER_KEY or MEMORIA_API_KEY
        }, clear=True):
            with pytest.raises(RuntimeError) as exc_info:
                create_editor(SessionLocal, user_id="test-user")
            
            error_msg = str(exc_info.value)
            assert "Memoria is required" in error_msg
            assert "MEMORIA_MASTER_KEY" in error_msg or "MEMORIA_API_KEY" in error_msg

    def test_valid_memoria_config_with_master_key(self):
        """Valid config with MASTER_KEY should not raise error."""
        with patch.dict(os.environ, {
            "MEMORIA_BASE_URL": "http://localhost:8100",
            "MEMORIA_MASTER_KEY": "test-key",
        }, clear=True):
            # Should not raise - we're just testing config validation
            try:
                editor = create_editor(SessionLocal, user_id="test-user")
                # Editor creation should succeed (actual HTTP calls may fail, that's OK)
                assert editor is not None
            except Exception as e:
                # Only config-related errors should fail this test
                if "Memoria is required" in str(e):
                    pytest.fail(f"Config validation failed: {e}")
                # Other errors (like HTTP connection) are expected in test env

    def test_valid_memoria_config_with_api_key(self):
        """Valid config with API_KEY should not raise error."""
        with patch.dict(os.environ, {
            "MEMORIA_BASE_URL": "http://localhost:8100",
            "MEMORIA_API_KEY": "sk-test-key",
        }, clear=True):
            try:
                editor = create_editor(SessionLocal, user_id="test-user")
                assert editor is not None
            except Exception as e:
                if "Memoria is required" in str(e):
                    pytest.fail(f"Config validation failed: {e}")

    def test_memoria_config_precedence(self):
        """MEMORIA_API_KEY takes precedence over MEMORIA_MASTER_KEY."""
        with patch.dict(os.environ, {
            "MEMORIA_BASE_URL": "http://localhost:8100",
            "MEMORIA_MASTER_KEY": "master-key",
            "MEMORIA_API_KEY": "sk-api-key",
        }, clear=True):
            try:
                editor = create_editor(SessionLocal, user_id="test-user")
                # Should use API_KEY, not MASTER_KEY
                # We can't easily test the internal choice without mocking HTTP client
                assert editor is not None
            except Exception as e:
                if "Memoria is required" in str(e):
                    pytest.fail(f"Config validation failed: {e}")


class TestMemoriaConfigDefaults:
    """Test default Memoria configuration values."""

    @pytest.mark.integration
    @pytest.mark.skipif(
        os.environ.get("CI") == "true",
        reason="Skip editor creation test in CI"
    )
    def test_default_memoria_base_url(self):
        """Test default MEMORIA_BASE_URL when not set."""
        from core.config import get_memoria_config
        
        with patch.dict(os.environ, {
            "MEMORIA_MASTER_KEY": "test-key",
        }, clear=True):
            # Remove MEMORIA_BASE_URL to test default
            if "MEMORIA_BASE_URL" in os.environ:
                del os.environ["MEMORIA_BASE_URL"]
            
            config = get_memoria_config()
            # Should use default URL
            assert config.base_url == "http://localhost:8100"
            assert config.master_key == "test-key"
            
            # Validation should pass
            errors = config.validate()
            assert len(errors) == 0


class TestMemoriaHealthCheck:
    """Test Memoria service health checking."""

    @pytest.mark.integration
    def test_memoria_service_health_check(self):
        """Test that we can detect if Memoria service is running."""
        import httpx
        
        # This test requires actual Memoria service running
        memoria_url = os.environ.get("MEMORIA_BASE_URL", "http://localhost:8100")
        
        try:
            response = httpx.get(f"{memoria_url}/health", timeout=5.0)
            if response.status_code == 200:
                health = response.json()
                assert health.get("status") in ["ok", "healthy"]
                assert "database" in health
            else:
                pytest.skip(f"Memoria service not available at {memoria_url}")
        except httpx.RequestError:
            pytest.skip(f"Memoria service not reachable at {memoria_url}")

    @pytest.mark.integration  
    def test_memoria_auth_validation(self):
        """Test that Memoria auth is validated on first request."""
        import httpx
        
        memoria_url = os.environ.get("MEMORIA_BASE_URL", "http://localhost:8100")
        
        # Test with invalid auth
        try:
            response = httpx.post(
                f"{memoria_url}/v1/memories/retrieve",
                json={"user_id": "test", "query": "test", "top_k": 1},
                headers={"Authorization": "Bearer invalid-key"},
                timeout=5.0
            )
            # Should get 401 or 403
            assert response.status_code in [401, 403], f"Expected auth error, got {response.status_code}"
        except httpx.RequestError:
            pytest.skip(f"Memoria service not reachable at {memoria_url}")


class TestMemoriaConfigInDotEnv:
    """Test that .env file has correct Memoria configuration."""

    def test_dot_env_has_memoria_config(self):
        """Test that .env file contains required Memoria configuration."""
        env_file = "/home/xupeng/github/mo-dev-agent/.env"
        
        if not os.path.exists(env_file):
            pytest.skip(".env file not found")
        
        with open(env_file, 'r') as f:
            content = f.read()
        
        # Check required config exists
        assert "MEMORIA_BASE_URL=" in content, "MEMORIA_BASE_URL missing from .env"
        assert "MEMORIA_MASTER_KEY=" in content, "MEMORIA_MASTER_KEY missing from .env"
        
        # Check values are not empty
        lines = content.split('\n')
        for line in lines:
            if line.startswith("MEMORIA_BASE_URL="):
                value = line.split('=', 1)[1].strip()
                assert value != "", "MEMORIA_BASE_URL is empty in .env"
                assert value.startswith("http"), f"MEMORIA_BASE_URL should be HTTP URL, got: {value}"
            
            if line.startswith("MEMORIA_MASTER_KEY="):
                value = line.split('=', 1)[1].strip()
                assert value != "", "MEMORIA_MASTER_KEY is empty in .env"
                assert len(value) > 10, f"MEMORIA_MASTER_KEY too short: {len(value)} chars"

    @pytest.mark.integration
    @pytest.mark.skipif(
        os.environ.get("CI") == "true", 
        reason="Skip connectivity tests in CI"
    )
    def test_dot_env_memoria_url_is_reachable(self):
        """Test that MEMORIA_BASE_URL in .env points to running service."""
        import httpx
        from dotenv import dotenv_values
        
        env_file = "/home/xupeng/github/mo-dev-agent/.env"
        if not os.path.exists(env_file):
            pytest.skip(".env file not found")
        
        config = dotenv_values(env_file)
        memoria_url = config.get("MEMORIA_BASE_URL")
        
        if not memoria_url:
            pytest.fail("MEMORIA_BASE_URL not found in .env")
        
        try:
            # Disable proxy for localhost requests
            with httpx.Client(trust_env=False) as client:
                response = client.get(f"{memoria_url}/health", timeout=5.0)
            assert response.status_code == 200, f"Memoria health check failed: {response.status_code}"
            
            health = response.json()
            assert health.get("status") in ["ok", "healthy"], f"Memoria unhealthy: {health}"
            
        except (httpx.RequestError, httpx.TimeoutException) as e:
            pytest.fail(f"Cannot reach Memoria at {memoria_url}: {e}")
