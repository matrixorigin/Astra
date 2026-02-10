"""Application configuration management.

Loads configuration from environment variables using Pydantic Settings.
Follows 12-factor app principles for configuration management.
"""

from functools import lru_cache

from pydantic import Field
from pydantic_settings import BaseSettings, SettingsConfigDict


class Settings(BaseSettings):
    """Application settings loaded from environment variables."""

    model_config = SettingsConfigDict(
        env_file=".env",
        env_file_encoding="utf-8",
        case_sensitive=False,
        extra="ignore",
    )

    # MatrixOne Database
    matrixone_host: str = Field(default="localhost", description="MatrixOne host")
    matrixone_port: int = Field(default=6001, description="MatrixOne port")
    matrixone_user: str = Field(default="root", description="MatrixOne user")
    matrixone_password: str = Field(default="111", description="MatrixOne password")
    matrixone_database: str = Field(default="dev_agent", description="Database name")

    # Redis
    redis_host: str = Field(default="localhost", description="Redis host")
    redis_port: int = Field(default=6379, description="Redis port")
    redis_password: str | None = Field(default=None, description="Redis password")

    # Application
    app_env: str = Field(default="development", description="Environment")
    log_level: str = Field(default="DEBUG", description="Log level")
    secret_key: str = Field(
        default="dev-secret-key-change-in-production",
        description="Secret key for encryption",
    )

    @property
    def is_development(self) -> bool:
        """Check if running in development mode."""
        return self.app_env == "development"

    @property
    def is_production(self) -> bool:
        """Check if running in production mode."""
        return self.app_env == "production"


@lru_cache
def get_settings() -> Settings:
    """Get cached settings instance.

    Returns:
        Settings: Application settings
    """
    return Settings()
