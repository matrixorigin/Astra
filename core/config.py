"""Configuration management with environment support."""

import os
from enum import Enum

from pydantic import BaseModel, Field
from pydantic_settings import BaseSettings, SettingsConfigDict

from core.exceptions import ConfigurationError
from core.logging_config import get_logger

logger = get_logger(__name__)


class Environment(str, Enum):
    """Deployment environments."""

    DEVELOPMENT = "development"
    STAGING = "staging"
    PRODUCTION = "production"
    TEST = "test"


class DatabaseConfig(BaseModel):
    """Database configuration."""

    host: str = Field(default="localhost")
    port: int = Field(default=6001)
    user: str = Field(default="root")
    password: str = Field(default="111")
    database: str = Field(default="dev_agent")
    pool_size: int = Field(default=10)
    max_overflow: int = Field(default=20)


class LLMConfig(BaseModel):
    """LLM configuration."""

    provider: str = Field(default="openai")
    model: str = Field(default="gpt-4")
    temperature: float = Field(default=0.7)
    max_tokens: int = Field(default=2000)
    timeout: int = Field(default=30)
    max_retries: int = Field(default=3)


class SecurityConfig(BaseModel):
    """Security configuration."""

    api_key_header: str = Field(default="X-API-Key")
    jwt_secret: str | None = Field(default=None)
    jwt_algorithm: str = Field(default="HS256")
    jwt_expiration: int = Field(default=3600)  # seconds
    rate_limit_per_minute: int = Field(default=60)
    allowed_origins: list[str] = Field(default=["*"])


class MonitoringConfig(BaseModel):
    """Monitoring configuration."""

    enable_metrics: bool = Field(default=True)
    enable_tracing: bool = Field(default=False)
    health_check_interval: int = Field(default=30)  # seconds


class Settings(BaseSettings):
    """Application settings."""

    # Environment
    environment: Environment = Field(default=Environment.DEVELOPMENT)
    debug: bool = Field(default=False)
    version: str = Field(default="0.2.0")

    # Server
    host: str = Field(default="0.0.0.0")
    port: int = Field(default=8000)
    workers: int = Field(default=1)

    # Logging
    log_level: str = Field(default="INFO")
    log_format: str = Field(default="json")  # json or text

    # Database
    database: DatabaseConfig = Field(default_factory=DatabaseConfig)

    # LLM
    llm: LLMConfig = Field(default_factory=LLMConfig)

    # Security
    security: SecurityConfig = Field(default_factory=SecurityConfig)

    # Monitoring
    monitoring: MonitoringConfig = Field(default_factory=MonitoringConfig)

    # Secrets (loaded from environment or secrets manager)
    openai_api_key: str | None = Field(default=None)
    groq_api_key: str | None = Field(default=None)
    github_token: str | None = Field(default=None)

    model_config = SettingsConfigDict(
        env_file=".env",
        env_file_encoding="utf-8",
        env_nested_delimiter="__",
        case_sensitive=False,
        extra="ignore",  # Ignore extra fields from .env
    )

    @classmethod
    def load(cls, env: str | None = None) -> "Settings":
        """Load settings for environment.

        Args:
            env: Environment name (development, staging, production)

        Returns:
            Settings instance
        """
        if env is None:
            env = os.getenv("ENVIRONMENT", "development")

        logger.info(f"Loading configuration for environment: {env}")

        try:
            # Load base settings
            settings = cls()

            # Override with environment-specific settings
            settings.environment = Environment(env)

            # Production-specific overrides
            if settings.environment == Environment.PRODUCTION:
                settings.debug = False
                settings.log_level = "WARNING"
                settings.security.allowed_origins = ["https://app.example.com"]  # TODO: Configure
                settings.monitoring.enable_tracing = True

            # Staging-specific overrides
            elif settings.environment == Environment.STAGING:
                settings.debug = False
                settings.log_level = "INFO"

            # Test-specific overrides
            elif settings.environment == Environment.TEST:
                settings.database.database = "dev_agent_test"
                settings.log_level = "DEBUG"

            # Validate required secrets in production
            if settings.environment == Environment.PRODUCTION:
                settings._validate_production_secrets()

            logger.info(f"Configuration loaded successfully for {env}")
            return settings

        except Exception as e:
            logger.error(f"Failed to load configuration: {e}")
            raise ConfigurationError(f"Failed to load configuration: {e}") from e

    def _validate_production_secrets(self) -> None:
        """Validate required secrets in production."""
        required_secrets = []

        if not self.security.jwt_secret:
            required_secrets.append("JWT_SECRET")

        if not self.openai_api_key and self.llm.provider == "openai":
            required_secrets.append("OPENAI_API_KEY")

        if not self.github_token:
            required_secrets.append("GITHUB_TOKEN")

        if required_secrets:
            raise ConfigurationError(
                f"Missing required secrets in production: {', '.join(required_secrets)}"
            )

    def get_database_url(self) -> str:
        """Get database connection URL."""
        return (
            f"mysql+pymysql://{self.database.user}:{self.database.password}"
            f"@{self.database.host}:{self.database.port}/{self.database.database}"
        )

    def is_production(self) -> bool:
        """Check if running in production."""
        return self.environment == Environment.PRODUCTION

    def is_development(self) -> bool:
        """Check if running in development."""
        return self.environment == Environment.DEVELOPMENT


# Global settings instance
_settings: Settings | None = None


def get_settings() -> Settings:
    """Get global settings instance.

    Returns:
        Settings instance
    """
    global _settings
    if _settings is None:
        _settings = Settings.load()
    return _settings


def reload_settings(env: str | None = None) -> Settings:
    """Reload settings.

    Args:
        env: Environment name

    Returns:
        New settings instance
    """
    global _settings
    _settings = Settings.load(env)
    return _settings
