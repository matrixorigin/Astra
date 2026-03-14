"""Tests for JWT token management."""

import time
from datetime import datetime, timezone

import pytest
from jwt.exceptions import ExpiredSignatureError, InvalidTokenError

from core.auth.jwt_manager import (
    JWTConfig,
    create_access_token,
    create_refresh_token,
    decode_token,
    verify_token_type,
)


class TestJWTTokenCreation:
    """Test JWT token creation."""

    def test_create_access_token(self):
        """Test creating access token."""
        token = create_access_token({"sub": "user_123", "username": "alice"})
        assert isinstance(token, str)
        assert len(token) > 0

    def test_create_refresh_token(self):
        """Test creating refresh token."""
        token = create_refresh_token({"sub": "user_123"})
        assert isinstance(token, str)
        assert len(token) > 0

    def test_access_token_contains_required_claims(self):
        """Test that access token contains required claims."""
        token = create_access_token({"sub": "user_123", "username": "alice"})
        payload = decode_token(token)

        assert payload["sub"] == "user_123"
        assert payload["username"] == "alice"
        assert payload["type"] == "access"
        assert "exp" in payload
        assert "iat" in payload

    def test_refresh_token_contains_required_claims(self):
        """Test that refresh token contains required claims."""
        token = create_refresh_token({"sub": "user_123"})
        payload = decode_token(token)

        assert payload["sub"] == "user_123"
        assert payload["type"] == "refresh"
        assert "exp" in payload
        assert "iat" in payload

    def test_access_token_expiry(self):
        """Test that access token has correct expiry."""
        config = JWTConfig()
        config.access_token_expire_minutes = 60

        token = create_access_token({"sub": "user_123"}, config)
        payload = decode_token(token, config)

        exp = datetime.fromtimestamp(payload["exp"], tz=timezone.utc)
        iat = datetime.fromtimestamp(payload["iat"], tz=timezone.utc)

        # Should expire in approximately 60 minutes
        delta = (exp - iat).total_seconds()
        assert 3590 < delta < 3610  # Allow 10 second tolerance

    def test_refresh_token_expiry(self):
        """Test that refresh token has correct expiry."""
        config = JWTConfig()
        config.refresh_token_expire_days = 7

        token = create_refresh_token({"sub": "user_123"}, config)
        payload = decode_token(token, config)

        exp = datetime.fromtimestamp(payload["exp"], tz=timezone.utc)
        iat = datetime.fromtimestamp(payload["iat"], tz=timezone.utc)

        # Should expire in approximately 7 days
        delta = (exp - iat).total_seconds()
        expected = 7 * 24 * 3600
        assert expected - 10 < delta < expected + 10


class TestJWTTokenDecoding:
    """Test JWT token decoding."""

    def test_decode_valid_token(self):
        """Test decoding valid token."""
        token = create_access_token({"sub": "user_123", "username": "alice"})
        payload = decode_token(token)

        assert payload["sub"] == "user_123"
        assert payload["username"] == "alice"

    def test_decode_invalid_token(self):
        """Test decoding invalid token raises error."""
        with pytest.raises(InvalidTokenError):
            decode_token("invalid_token")

    def test_decode_expired_token(self):
        """Test decoding expired token raises error."""
        config = JWTConfig()
        config.access_token_expire_minutes = 0  # Expire immediately

        token = create_access_token({"sub": "user_123"}, config)
        time.sleep(1)  # Wait for token to expire

        with pytest.raises(ExpiredSignatureError):
            decode_token(token, config)

    def test_decode_tampered_token(self):
        """Test decoding tampered token raises error."""
        token = create_access_token({"sub": "user_123"})
        # Tamper with token by modifying the signature part (after last dot)
        parts = token.rsplit(".", 1)
        if len(parts) == 2:
            # Change a character in the middle of the signature
            sig = parts[1]
            if len(sig) > 5:
                tampered_sig = sig[:5] + ("x" if sig[5] != "x" else "y") + sig[6:]
                tampered_token = parts[0] + "." + tampered_sig
            else:
                tampered_token = token[:-1] + ("a" if token[-1] != "a" else "b")
        else:
            tampered_token = token + "tampered"

        with pytest.raises(InvalidTokenError):
            decode_token(tampered_token)


class TestTokenTypeVerification:
    """Test token type verification."""

    def test_verify_access_token_type(self):
        """Test verifying access token type."""
        token = create_access_token({"sub": "user_123"})
        payload = decode_token(token)

        assert verify_token_type(payload, "access") is True
        assert verify_token_type(payload, "refresh") is False

    def test_verify_refresh_token_type(self):
        """Test verifying refresh token type."""
        token = create_refresh_token({"sub": "user_123"})
        payload = decode_token(token)

        assert verify_token_type(payload, "refresh") is True
        assert verify_token_type(payload, "access") is False

    def test_verify_token_type_missing(self):
        """Test verifying token with missing type."""
        # Create token without type (manually)
        import jwt

        config = JWTConfig()
        token = jwt.encode({"sub": "user_123"}, config.secret_key, algorithm=config.algorithm)
        payload = decode_token(token, config)

        assert verify_token_type(payload, "access") is False
        assert verify_token_type(payload, "refresh") is False


class TestJWTConfig:
    """Test JWT configuration."""

    def test_jwt_config_defaults(self):
        """Test JWT config loads defaults."""
        config = JWTConfig()

        assert config.secret_key is not None
        assert config.algorithm == "HS256"
        assert config.access_token_expire_minutes > 0
        assert config.refresh_token_expire_days > 0

    def test_jwt_config_from_env(self, monkeypatch):
        """Test JWT config loads from environment."""
        # Use a 32+ byte secret to avoid padding
        monkeypatch.setenv("JWT_SECRET_KEY", "test_secret_key_with_32_bytes_min")
        monkeypatch.setenv("JWT_ALGORITHM", "HS512")
        monkeypatch.setenv("JWT_ACCESS_TOKEN_EXPIRE_MINUTES", "30")
        monkeypatch.setenv("JWT_REFRESH_TOKEN_EXPIRE_DAYS", "14")

        config = JWTConfig()

        assert config.secret_key == "test_secret_key_with_32_bytes_min"
        assert config.algorithm == "HS512"
        assert config.access_token_expire_minutes == 30
        assert config.refresh_token_expire_days == 14
