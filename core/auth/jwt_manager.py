"""JWT token generation and validation."""

import os
from datetime import datetime, timedelta, timezone
from typing import Any

import jwt
from jwt.exceptions import InvalidTokenError

from core.logging_config import get_logger

logger = get_logger(__name__)


class JWTConfig:
    """JWT configuration from environment."""

    def __init__(self):
        secret = os.getenv("JWT_SECRET_KEY", "change-me-in-production")
        # Ensure minimum 32 bytes for HS256
        if len(secret) < 32:
            secret = secret.ljust(32, "0")
        self.secret_key = secret
        self.algorithm = os.getenv("JWT_ALGORITHM", "HS256")
        self.access_token_expire_minutes = int(
            os.getenv("JWT_ACCESS_TOKEN_EXPIRE_MINUTES", "60")
        )
        self.refresh_token_expire_days = int(
            os.getenv("JWT_REFRESH_TOKEN_EXPIRE_DAYS", "7")
        )


def create_access_token(data: dict[str, Any], config: JWTConfig | None = None) -> str:
    """Create JWT access token.

    Args:
        data: Token payload (must include 'sub' for user_id)
        config: JWT configuration. If None, loads from environment.

    Returns:
        Encoded JWT token

    Example:
        >>> token = create_access_token({"sub": "user_123", "username": "alice"})
        >>> payload = decode_token(token)
        >>> payload["sub"]
        'user_123'
    """
    config = config or JWTConfig()
    to_encode = data.copy()

    # Add standard claims
    now = datetime.now(timezone.utc)
    expire = now + timedelta(minutes=config.access_token_expire_minutes)
    to_encode.update({
        "exp": expire,
        "iat": now,
        "type": "access"
    })

    encoded_jwt = jwt.encode(to_encode, config.secret_key, algorithm=config.algorithm)
    return encoded_jwt


def create_refresh_token(data: dict[str, Any], config: JWTConfig | None = None) -> str:
    """Create JWT refresh token.

    Args:
        data: Token payload (must include 'sub' for user_id)
        config: JWT configuration. If None, loads from environment.

    Returns:
        Encoded JWT token

    Example:
        >>> token = create_refresh_token({"sub": "user_123"})
        >>> payload = decode_token(token)
        >>> payload["type"]
        'refresh'
    """
    config = config or JWTConfig()
    to_encode = data.copy()

    # Add standard claims
    now = datetime.now(timezone.utc)
    expire = now + timedelta(days=config.refresh_token_expire_days)
    to_encode.update({
        "exp": expire,
        "iat": now,
        "type": "refresh"
    })

    encoded_jwt = jwt.encode(to_encode, config.secret_key, algorithm=config.algorithm)
    return encoded_jwt


def decode_token(token: str, config: JWTConfig | None = None) -> dict[str, Any]:
    """Decode and validate JWT token.

    Args:
        token: JWT token
        config: JWT configuration. If None, loads from environment.

    Returns:
        Token payload

    Raises:
        InvalidTokenError: If token is invalid or expired

    Example:
        >>> token = create_access_token({"sub": "user_123"})
        >>> payload = decode_token(token)
        >>> payload["sub"]
        'user_123'
    """
    config = config or JWTConfig()
    try:
        payload: dict[str, Any] = jwt.decode(token, config.secret_key, algorithms=[config.algorithm])
        return payload
    except InvalidTokenError as e:
        logger.warning(f"Invalid token: {e}")
        raise


def verify_token_type(payload: dict[str, Any], expected_type: str) -> bool:
    """Verify token type.

    Args:
        payload: Decoded token payload
        expected_type: Expected token type ('access' or 'refresh')

    Returns:
        True if token type matches, False otherwise

    Example:
        >>> token = create_access_token({"sub": "user_123"})
        >>> payload = decode_token(token)
        >>> verify_token_type(payload, "access")
        True
        >>> verify_token_type(payload, "refresh")
        False
    """
    return payload.get("type") == expected_type
