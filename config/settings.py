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
    
    # Embedding
    embedding_provider: str = Field(default="local", description="Embedding provider: local, openai, mock")
    embedding_model: str = Field(default="all-MiniLM-L6-v2", description="Embedding model name")
    embedding_dim: int = Field(default=384, description="Embedding vector dimension")
    embedding_api_key: str = Field(default="", description="API key for openai-compatible embedding")
    embedding_base_url: str | None = Field(default=None, description="Base URL for openai-compatible embedding")

    # External Services
    github_token: str | None = Field(default=None, description="GitHub API token")

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
