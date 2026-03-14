"""Configuration management for mo-dev-agent.

This module provides centralized configuration management with clear separation
between development, test, and production environments.
"""

import os
from dataclasses import dataclass
from typing import Optional


@dataclass
class MemoriaConfig:
    """Memoria service configuration."""
    base_url: str
    master_key: Optional[str] = None
    api_key: Optional[str] = None
    
    @property
    def auth_key(self) -> Optional[str]:
        """Get the authentication key (API key takes precedence)."""
        return self.api_key or self.master_key
    
    def validate(self) -> list[str]:
        """Validate configuration and return list of errors."""
        errors = []
        
        if not self.base_url:
            errors.append("Memoria base URL is required")
        elif not self.base_url.startswith(("http://", "https://")):
            errors.append(f"Memoria base URL must be HTTP/HTTPS, got: {self.base_url}")
        
        if not self.auth_key:
            errors.append("Memoria authentication required (master_key or api_key)")
        
        return errors


@dataclass
class AppConfig:
    """Application configuration."""
    memoria: MemoriaConfig
    environment: str = "development"
    
    def validate(self) -> list[str]:
        """Validate all configuration."""
        errors = []
        errors.extend(self.memoria.validate())
        return errors


def get_memoria_config() -> MemoriaConfig:
    """Get Memoria configuration from environment variables.
    
    Environment variable precedence:
    1. Test environment: TEST_MEMORIA_* (for tests only)
    2. Production environment: MEMORIA_*
    """
    # Load .env file if available
    try:
        from dotenv import load_dotenv
        load_dotenv()
    except ImportError:
        pass  # dotenv not available
    
    # Check if we're in test environment
    is_test = (
        "pytest" in os.environ.get("_", "") or
        "PYTEST_CURRENT_TEST" in os.environ or
        os.environ.get("TESTING") == "1"
    )
    
    if is_test:
        # Test configuration
        base_url = os.environ.get("TEST_MEMORIA_BASE_URL", "http://localhost:8100")
        master_key = os.environ.get("TEST_MEMORIA_MASTER_KEY", "test-master-key-for-docker-compose")
        api_key = os.environ.get("TEST_MEMORIA_API_KEY")
    else:
        # Production/development configuration
        base_url = os.environ.get("MEMORIA_BASE_URL", "http://localhost:8100")
        master_key = os.environ.get("MEMORIA_MASTER_KEY")
        api_key = os.environ.get("MEMORIA_API_KEY")
    
    return MemoriaConfig(
        base_url=base_url,
        master_key=master_key,
        api_key=api_key
    )


def get_app_config() -> AppConfig:
    """Get complete application configuration."""
    environment = os.environ.get("ENVIRONMENT", "development")
    
    return AppConfig(
        memoria=get_memoria_config(),
        environment=environment
    )


# Global config instance (lazy loaded)
_config: Optional[AppConfig] = None


def get_config() -> AppConfig:
    """Get global configuration instance."""
    global _config
    if _config is None:
        _config = get_app_config()
    return _config


def reset_config():
    """Reset global configuration (for testing)."""
    global _config
    _config = None
