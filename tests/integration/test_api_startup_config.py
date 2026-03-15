"""Test API startup configuration validation."""

import os
import pytest
from unittest.mock import patch
from fastapi.testclient import TestClient


class TestAPIStartupConfigValidation:
    """Test that API startup validates configuration correctly."""

    def test_api_startup_logs_config_validation(self, caplog):
        """Test that API startup runs configuration validation."""
        # Import here to avoid loading the app during collection
        from api.main import app

        with TestClient(app) as client:
            # Just creating the client triggers startup
            pass

        # Check that config validation was logged
        log_messages = [record.message for record in caplog.records]
        assert any("Validating startup configuration" in msg for msg in log_messages), (
            f"Config validation not logged. Messages: {log_messages}"
        )

    def test_api_continues_with_config_warnings(self, caplog):
        """Test that API continues startup even with configuration warnings."""
        # Mock a config warning scenario
        with patch("core.config_validation.validate_memoria_connectivity") as mock_connectivity:
            mock_connectivity.return_value = ["Memoria service not reachable"]

            from api.main import app

            with TestClient(app) as client:
                # API should still start despite warnings
                response = client.get("/health")
                assert response.status_code == 200

        # Should log the warning
        log_messages = [record.message for record in caplog.records]
        assert any("Config warning" in msg for msg in log_messages), (
            f"Config warning not logged. Messages: {log_messages}"
        )

    @pytest.mark.integration
    def test_api_health_endpoint_works_after_startup(self):
        """Test that API health endpoint works after startup validation."""
        from api.main import app

        with TestClient(app) as client:
            response = client.get("/health")
            assert response.status_code == 200

            health = response.json()
            assert "status" in health
            assert health["status"] in ["healthy", "ok"]


class TestConfigValidationIntegration:
    """Test configuration validation integration with real services."""

    @pytest.mark.integration
    def test_config_validation_detects_memoria_issues(self):
        """Test that config validation detects real Memoria connectivity issues."""
        from core import config as cfg_mod
        from core.config_validation import validate_memoria_connectivity

        # Clear test env detection so our invalid URL is actually used
        cfg_mod.reset_config()
        with patch.dict(os.environ, {"MEMORIA_BASE_URL": "http://invalid-host:9999"}, clear=False):
            os.environ.pop("PYTEST_CURRENT_TEST", None)
            errors = validate_memoria_connectivity()
            assert len(errors) > 0

    @pytest.mark.integration
    def test_config_validation_passes_with_good_config(self):
        """Test that config validation passes with correct configuration."""
        from core.config_validation import validate_all_startup_config

        # Should pass with current .env configuration
        errors, warnings = validate_all_startup_config()

        # No hard errors should be present
        assert len(errors) == 0, f"Unexpected config errors: {errors}"

        # Warnings are OK (e.g., connectivity issues in test environment)
        if warnings:
            print(f"Config warnings (expected in test): {warnings}")

    def test_config_validation_script_works(self):
        """Test that config validation script can be run directly."""
        import subprocess
        import sys

        # Run the config validation script
        result = subprocess.run(
            [sys.executable, "core/config_validation.py"],
            capture_output=True,
            text=True,
            cwd="/home/xupeng/github/mo-dev-agent",
        )

        # Should exit successfully (warnings are OK)
        assert result.returncode == 0, f"Config validation failed: {result.stderr}"
        assert "✅ All configuration valid" in result.stdout
