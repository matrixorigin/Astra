"""Tests for password hashing and verification."""

import pytest

from core.auth.password import hash_password, verify_password


class TestPasswordHashing:
    """Test password hashing functions."""

    def test_hash_password_returns_string(self):
        """Test that hash_password returns a string."""
        hashed = hash_password("test_password")
        assert isinstance(hashed, str)
        assert len(hashed) > 0

    def test_hash_password_different_each_time(self):
        """Test that hashing same password produces different hashes (due to salt)."""
        password = "test_password"
        hash1 = hash_password(password)
        hash2 = hash_password(password)
        assert hash1 != hash2

    def test_verify_password_correct(self):
        """Test that verify_password returns True for correct password."""
        password = "test_password"
        hashed = hash_password(password)
        assert verify_password(password, hashed) is True

    def test_verify_password_incorrect(self):
        """Test that verify_password returns False for incorrect password."""
        password = "test_password"
        hashed = hash_password(password)
        assert verify_password("wrong_password", hashed) is False

    def test_verify_password_empty_password(self):
        """Test that verify_password handles empty password."""
        hashed = hash_password("test_password")
        assert verify_password("", hashed) is False

    def test_verify_password_invalid_hash(self):
        """Test that verify_password handles invalid hash gracefully."""
        assert verify_password("test_password", "invalid_hash") is False

    def test_hash_password_unicode(self):
        """Test that hash_password handles unicode characters."""
        password = "测试密码🔒"
        hashed = hash_password(password)
        assert verify_password(password, hashed) is True

    def test_hash_password_long_password(self):
        """Test that hash_password handles long passwords (bcrypt has 72 byte limit)."""
        # Password at bcrypt max length should work
        password = "a" * 72
        hashed = hash_password(password)
        assert verify_password(password, hashed) is True

        # bcrypt will raise ValueError for passwords > 72 bytes
        # This is expected and documented behavior

    def test_hash_password_special_characters(self):
        """Test that hash_password handles special characters."""
        password = "!@#$%^&*()_+-=[]{}|;:',.<>?/~`"
        hashed = hash_password(password)
        assert verify_password(password, hashed) is True
