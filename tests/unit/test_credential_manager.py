"""Tests for CredentialManager — Fernet encryption/decryption."""

import pytest

from core.skills.credential_manager import CredentialManager


class TestCredentialManager:
    def test_encrypt_decrypt_roundtrip(self):
        mgr = CredentialManager("test-secret-key")
        plaintext = "ghp_abc123XYZ"
        encrypted = mgr.encrypt(plaintext)
        assert encrypted != plaintext
        assert mgr.decrypt(encrypted) == plaintext

    def test_different_keys_cannot_decrypt(self):
        mgr1 = CredentialManager("key-one")
        mgr2 = CredentialManager("key-two")
        encrypted = mgr1.encrypt("secret")
        with pytest.raises(Exception):
            mgr2.decrypt(encrypted)

    def test_same_key_different_instances(self):
        mgr1 = CredentialManager("same-key")
        mgr2 = CredentialManager("same-key")
        encrypted = mgr1.encrypt("token-value")
        assert mgr2.decrypt(encrypted) == "token-value"

    def test_encrypt_produces_different_ciphertexts(self):
        """Fernet uses random IV, so same plaintext → different ciphertext."""
        mgr = CredentialManager("key")
        c1 = mgr.encrypt("same")
        c2 = mgr.encrypt("same")
        assert c1 != c2

    def test_empty_string(self):
        mgr = CredentialManager("key")
        assert mgr.decrypt(mgr.encrypt("")) == ""

    def test_unicode_value(self):
        mgr = CredentialManager("key")
        val = "密码🔑"
        assert mgr.decrypt(mgr.encrypt(val)) == val

    def test_long_secret_key(self):
        mgr = CredentialManager("a" * 1000)
        assert mgr.decrypt(mgr.encrypt("ok")) == "ok"
