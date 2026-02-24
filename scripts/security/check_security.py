#!/usr/bin/env python3
"""Pre-deployment security check script."""

import os
import sys


def check_encryption_key():
    """Check if TOKEN_ENCRYPTION_KEY is properly set."""
    key = os.getenv("TOKEN_ENCRYPTION_KEY")
    
    if not key:
        print("❌ CRITICAL: TOKEN_ENCRYPTION_KEY is not set!")
        print("   Generate a key with:")
        print("   python -c \"from cryptography.fernet import Fernet; print(Fernet.generate_key().decode())\"")
        return False
    
    # Check for placeholder values
    unsafe_values = [
        "CHANGE_ME_IN_PRODUCTION",
        "MUST_BE_SET_IN_PRODUCTION_OR_APP_WILL_FAIL",
        "test-encryption-key",
        "default",
    ]
    
    if any(unsafe in key for unsafe in unsafe_values):
        print(f"❌ CRITICAL: TOKEN_ENCRYPTION_KEY contains unsafe placeholder: {key[:20]}...")
        print("   Generate a secure key with:")
        print("   python -c \"from cryptography.fernet import Fernet; print(Fernet.generate_key().decode())\"")
        return False
    
    if len(key) < 32:
        print(f"⚠️  WARNING: TOKEN_ENCRYPTION_KEY is too short ({len(key)} chars, recommended: 44+)")
        return False
    
    print(f"✅ TOKEN_ENCRYPTION_KEY is set ({len(key)} chars)")
    return True


def check_jwt_secret():
    """Check if JWT_SECRET_KEY is properly set."""
    key = os.getenv("JWT_SECRET_KEY")
    
    if not key:
        print("❌ CRITICAL: JWT_SECRET_KEY is not set!")
        return False
    
    unsafe_values = [
        "your-secret-key",
        "change-in-production",
        "your-super-secret-key",
    ]
    
    if any(unsafe in key for unsafe in unsafe_values):
        print(f"❌ CRITICAL: JWT_SECRET_KEY contains unsafe placeholder")
        return False
    
    if len(key) < 32:
        print(f"⚠️  WARNING: JWT_SECRET_KEY is too short ({len(key)} chars, recommended: 32+)")
        return False
    
    print(f"✅ JWT_SECRET_KEY is set ({len(key)} chars)")
    return True


def main():
    """Run all security checks."""
    print("🔒 Running pre-deployment security checks...\n")
    
    checks = [
        check_encryption_key(),
        check_jwt_secret(),
    ]
    
    if all(checks):
        print("\n✅ All security checks passed!")
        return 0
    else:
        print("\n❌ Security checks failed! Fix the issues above before deploying.")
        return 1


if __name__ == "__main__":
    sys.exit(main())
