#!/usr/bin/env python3
"""Initialize default prompt templates in database."""

from core.context.prompts import init_default_prompts
from api.database import SessionLocal

if __name__ == "__main__":
    print("Initializing default prompt templates...")
    init_default_prompts(SessionLocal)
    print("✓ Done! Default prompts registered.")

    # Verify
    from core.context.prompts import PromptManager

    manager = PromptManager(SessionLocal)

    print("\nRegistered prompts:")
    from sqlalchemy import text
    db = SessionLocal()
    result = db.execute(text("""
        SELECT template_id, version, is_active
        FROM prompt_templates
        ORDER BY template_id, version
    """))

    for row in result:
        status = "✓ active" if row.is_active else "  inactive"
        print(f"  {status} {row.template_id}@{row.version}")
    db.close()
