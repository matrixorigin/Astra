#!/usr/bin/env python3
"""Development environment initialization script.

Automatically:
1. Generate TOKEN_ENCRYPTION_KEY if missing
2. Fix common .env configuration errors
3. Validate configuration
"""

import os
import sys
from pathlib import Path
from cryptography.fernet import Fernet


def main():
    """Initialize development environment."""
    project_root = Path(__file__).parent.parent.parent  # scripts/dev/init.py -> project root
    env_file = project_root / ".env"

    if not env_file.exists():
        print("❌ .env file not found. Run 'make setup' first.")
        sys.exit(1)

    print("🔧 Initializing development environment...")
    print()

    # Read current .env
    content = env_file.read_text()
    lines = content.split("\n")
    modified = False

    # 1. Generate TOKEN_ENCRYPTION_KEY if missing
    if "TOKEN_ENCRYPTION_KEY" not in content or "CHANGE_ME" in content:
        key = Fernet.generate_key().decode()

        # Remove old placeholder if exists
        lines = [line for line in lines if not line.startswith("TOKEN_ENCRYPTION_KEY=")]
        lines.append(f"TOKEN_ENCRYPTION_KEY={key}")

        print("✅ Generated TOKEN_ENCRYPTION_KEY")
        modified = True
    else:
        print("✅ TOKEN_ENCRYPTION_KEY already configured")

    # 2. Generate JWT_SECRET_KEY if missing
    if "JWT_SECRET_KEY" not in content or "CHANGE_ME" in content:
        import secrets

        jwt_key = secrets.token_urlsafe(32)

        # Remove old placeholder if exists
        lines = [line for line in lines if not line.startswith("JWT_SECRET_KEY=")]
        lines.append(f"JWT_SECRET_KEY={jwt_key}")

        print("✅ Generated JWT_SECRET_KEY")
        modified = True
    else:
        print("✅ JWT_SECRET_KEY already configured")

    # 3. Validate LLM configuration
    llm_provider = None
    llm_model = None
    for line in lines:
        if line.startswith("LLM__PROVIDER="):
            llm_provider = line.split("=", 1)[1].strip()
        if line.startswith("LLM__MODEL="):
            llm_model = line.split("=", 1)[1].strip()

    if llm_provider and llm_model:
        # Check for common mismatches
        if llm_provider == "openai" and llm_model == "deepseek":
            print("⚠️  Warning: LLM__PROVIDER=openai but LLM__MODEL=deepseek")
            print("   Consider using: LLM__PROVIDER=deepseek or LLM__MODEL=gpt-4o")
        elif llm_provider == "deepseek" and llm_model.startswith("gpt-"):
            print("⚠️  Warning: LLM__PROVIDER=deepseek but LLM__MODEL=gpt-*")
            print("   Consider using: LLM__PROVIDER=openai or LLM__MODEL=deepseek-chat")
        else:
            print(f"✅ LLM configuration: {llm_provider}/{llm_model}")

    # Write back if modified
    if modified:
        env_file.write_text("\n".join(lines))
        print()
        print("✅ .env file updated")

    print()
    print("=" * 60)
    print("✅ Development environment initialized!")
    print("=" * 60)
    print()
    print("Next steps:")
    print("  1. Start services:  make dev-start")
    print("  2. Run setup:       make dev-setup-demo")
    print("  3. Start chatting:  mo-agent chat")


if __name__ == "__main__":
    main()
