#!/usr/bin/env python3
"""Initialize default prompt templates in database."""

from sdk import Database
from core.context.prompts import init_default_prompts

if __name__ == "__main__":
    db = Database()
    print("Initializing default prompt templates...")
    init_default_prompts(db)
    print("✓ Done! Default prompts registered.")
    
    # Verify
    from core.context.prompts import PromptManager
    manager = PromptManager(db)
    
    print("\nRegistered prompts:")
    rows = db.fetchall("""
        SELECT template_id, version, is_active
        FROM prompt_templates
        ORDER BY template_id, version
    """)
    
    for row in rows:
        status = "✓ active" if row['is_active'] else "  inactive"
        print(f"  {status} {row['template_id']}@{row['version']}")
