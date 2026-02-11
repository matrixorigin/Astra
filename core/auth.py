"""API authentication and authorization."""

from typing import Optional
from fastapi import Header, HTTPException, status
from datetime import datetime, UTC, timedelta
import jwt

from core.config import get_settings
from core.logging_config import get_logger
from core.exceptions import AuthenticationError, AuthorizationError

settings = get_settings()
logger = get_logger(__name__)


class APIKeyAuth:
    """API Key authentication."""

    def __init__(self):
        self.valid_keys = self._load_api_keys()

    def _load_api_keys(self) -> dict[str, dict]:
        """Load API keys from database or config.

        In production, load from database with user info.
        For now, use environment variable.
        """
        # TODO: Load from database
        # For now, accept any key in development
        if settings.is_development():
            return {"dev-key": {"user_id": "dev_user", "permissions": ["read", "write"]}}

        # In production, this should query database
        return {}

    async def verify(
        self, api_key: Optional[str] = Header(None, alias=settings.security.api_key_header)
    ) -> dict:
        """Verify API key.

        Args:
            api_key: API key from header

        Returns:
            User info dict

        Raises:
            AuthenticationError: If authentication fails
        """
        if not api_key:
            logger.warning("Missing API key")
            raise AuthenticationError("Missing API key")

        # Development mode: accept any key
        if settings.is_development():
            logger.debug(f"Development mode: accepting API key")
            return {"user_id": "dev_user", "permissions": ["read", "write"]}

        # Production mode: validate key
        user_info = self.valid_keys.get(api_key)
        if not user_info:
            logger.warning(f"Invalid API key")
            raise AuthenticationError("Invalid API key")

        logger.info(f"Authenticated user: {user_info['user_id']}")
        return user_info


class JWTAuth:
    """JWT token authentication."""

    def __init__(self):
        if not settings.security.jwt_secret:
            logger.warning("JWT secret not configured")

    def create_token(self, user_id: str, permissions: list[str] = None) -> str:
        """Create JWT token.

        Args:
            user_id: User identifier
            permissions: User permissions

        Returns:
            JWT token string
        """
        if not settings.security.jwt_secret:
            raise AuthenticationError("JWT not configured")

        payload = {
            "user_id": user_id,
            "permissions": permissions or [],
            "exp": datetime.now(UTC) + timedelta(seconds=settings.security.jwt_expiration),
            "iat": datetime.now(UTC),
        }

        token = jwt.encode(
            payload,
            settings.security.jwt_secret,
            algorithm=settings.security.jwt_algorithm,
        )

        logger.info(f"Created JWT token for user: {user_id}")
        return token

    async def verify(self, authorization: Optional[str] = Header(None)) -> dict:
        """Verify JWT token.

        Args:
            authorization: Authorization header (Bearer <token>)

        Returns:
            Token payload

        Raises:
            AuthenticationError: If authentication fails
        """
        if not authorization:
            raise AuthenticationError("Missing authorization header")

        if not authorization.startswith("Bearer "):
            raise AuthenticationError("Invalid authorization header format")

        token = authorization.replace("Bearer ", "")

        try:
            payload = jwt.decode(
                token,
                settings.security.jwt_secret,
                algorithms=[settings.security.jwt_algorithm],
            )

            logger.info(f"Authenticated user via JWT: {payload['user_id']}")
            return payload

        except jwt.ExpiredSignatureError:
            logger.warning("JWT token expired")
            raise AuthenticationError("Token expired")
        except jwt.InvalidTokenError as e:
            logger.warning(f"Invalid JWT token: {e}")
            raise AuthenticationError("Invalid token")


def require_permission(required_permission: str):
    """Decorator to require specific permission.

    Args:
        required_permission: Required permission (e.g., "write", "admin")
    """

    def decorator(user_info: dict):
        permissions = user_info.get("permissions", [])
        if required_permission not in permissions:
            logger.warning(
                f"User {user_info.get('user_id')} missing permission: {required_permission}"
            )
            raise AuthorizationError(f"Missing required permission: {required_permission}")
        return user_info

    return decorator


# Global instances
api_key_auth = APIKeyAuth()
jwt_auth = JWTAuth()
