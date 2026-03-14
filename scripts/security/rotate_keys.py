#!/usr/bin/env python3
"""
Rotate encryption keys and JWT secrets.

This script generates new keys and updates the .env file.
After rotation, all services must be restarted.
"""

import os
import sys
from pathlib import Path
from cryptography.fernet import Fernet
import secrets


def generate_fernet_key() -> str:
    """Generate a new Fernet encryption key."""
    return Fernet.generate_key().decode()


def generate_jwt_secret() -> str:
    """Generate a new JWT secret key."""
    return secrets.token_urlsafe(32)


def backup_env_file(env_path: Path) -> Path:
    """Create backup of .env file."""
    import shutil
    from datetime import datetime

    timestamp = datetime.now().strftime("%Y%m%d_%H%M%S")
    backup_path = env_path.parent / f".env.backup.{timestamp}"
    shutil.copy(env_path, backup_path)
    return backup_path


def rotate_keys(env_path: Path, dry_run: bool = False):
    """Rotate encryption keys and JWT secrets."""

    if not env_path.exists():
        print(f"❌ Error: .env file not found at {env_path}")
        sys.exit(1)

    # Read current .env
    with open(env_path, "r") as f:
        lines = f.readlines()

    # Generate new keys
    new_token_key = generate_fernet_key()
    new_jwt_secret = generate_jwt_secret()

    print("🔑 Key Rotation")
    print("=" * 60)
    print()

    if dry_run:
        print("🔍 DRY RUN MODE - No changes will be made")
        print()

    # Find and update keys
    updated_lines = []
    token_key_found = False
    jwt_secret_found = False

    for line in lines:
        if line.startswith("TOKEN_ENCRYPTION_KEY="):
            old_value = line.split("=", 1)[1].strip()
            if dry_run:
                print(f"Would update TOKEN_ENCRYPTION_KEY:")
                print(f"  Old: {old_value[:20]}...")
                print(f"  New: {new_token_key[:20]}...")
            else:
                updated_lines.append(f"TOKEN_ENCRYPTION_KEY={new_token_key}\n")
            token_key_found = True
        elif line.startswith("JWT_SECRET_KEY="):
            old_value = line.split("=", 1)[1].strip()
            if dry_run:
                print(f"Would update JWT_SECRET_KEY:")
                print(f"  Old: {old_value[:20]}...")
                print(f"  New: {new_jwt_secret[:20]}...")
            else:
                updated_lines.append(f"JWT_SECRET_KEY={new_jwt_secret}\n")
            jwt_secret_found = True
        else:
            updated_lines.append(line)

    if not token_key_found:
        print("⚠️  Warning: TOKEN_ENCRYPTION_KEY not found in .env")
        if not dry_run:
            updated_lines.append(f"TOKEN_ENCRYPTION_KEY={new_token_key}\n")

    if not jwt_secret_found:
        print("⚠️  Warning: JWT_SECRET_KEY not found in .env")
        if not dry_run:
            updated_lines.append(f"JWT_SECRET_KEY={new_jwt_secret}\n")

    if dry_run:
        print()
        print("✅ Dry run completed. Run without --dry-run to apply changes.")
        return

    # Create backup
    backup_path = backup_env_file(env_path)
    print()
    print(f"📦 Backup created: {backup_path}")

    # Write updated .env
    with open(env_path, "w") as f:
        f.writelines(updated_lines)

    print()
    print("✅ Keys rotated successfully!")
    print()
    print("⚠️  IMPORTANT: You must restart all services:")
    print("   make dev-restart")
    print()
    print("⚠️  WARNING: After rotation:")
    print("   • All existing JWT tokens will be invalid")
    print("   • Users must login again")
    print("   • Encrypted API tokens must be re-entered")
    print()
    print(f"📋 Backup saved to: {backup_path}")
    print("   To rollback: cp {backup_path} .env && make dev-restart")


def main():
    import argparse

    parser = argparse.ArgumentParser(description="Rotate encryption keys and JWT secrets")
    parser.add_argument(
        "--dry-run", action="store_true", help="Show what would be changed without making changes"
    )
    parser.add_argument("--env-file", default=".env", help="Path to .env file (default: .env)")

    args = parser.parse_args()

    # Get project root
    script_dir = Path(__file__).parent
    project_root = script_dir.parent.parent
    env_path = project_root / args.env_file

    rotate_keys(env_path, dry_run=args.dry_run)


if __name__ == "__main__":
    main()
