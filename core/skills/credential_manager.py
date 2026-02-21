"""Credential encryption/decryption using Fernet symmetric encryption."""

import base64
import hashlib

from cryptography.fernet import Fernet


class CredentialManager:
    """Encrypt/decrypt skill credentials using Fernet (AES-128-CBC)."""

    def __init__(self, secret_key: str):
        # Derive a 32-byte key from arbitrary-length secret_key
        key = hashlib.sha256(secret_key.encode()).digest()
        self._fernet = Fernet(base64.urlsafe_b64encode(key))

    def encrypt(self, plaintext: str) -> str:
        return self._fernet.encrypt(plaintext.encode()).decode()

    def decrypt(self, ciphertext: str) -> str:
        return self._fernet.decrypt(ciphertext.encode()).decode()
