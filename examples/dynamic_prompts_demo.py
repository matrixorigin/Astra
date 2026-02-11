#!/usr/bin/env python3
"""Demo: Dynamic prompt modification without restart."""

from core.context.manager import ContextManager, TaskType
from core.context.prompts import PromptManager
from sdk import Database


def demo_dynamic_prompts():
    """Demonstrate dynamic prompt modification."""
    db = Database()
    manager = PromptManager(db)
    context_mgr = ContextManager(db)

    print("=" * 60)
    print("Demo: Dynamic Prompt Modification")
    print("=" * 60)

    # 1. Get current prompt
    print("\n1. Current CODE_REVIEW prompt (v1.0):")
    print("-" * 60)
    prompt_v1 = context_mgr._get_system_prompt(TaskType.CODE_REVIEW)
    print(prompt_v1[:200] + "...")

    # 2. Register new version
    print("\n2. Registering improved version (v1.1)...")
    improved_prompt = """You are a senior code reviewer with 10+ years of experience.

Your review process:
1. Check for security vulnerabilities (OWASP Top 10)
2. Verify error handling and edge cases
3. Assess code readability and maintainability
4. Suggest performance optimizations
5. Ensure proper testing coverage

Focus: {focus}

Provide actionable feedback with code examples."""

    manager.register_prompt(
        template_id="system_code_review", version="1.1", content=improved_prompt, is_active=True
    )
    print("✓ Version 1.1 registered and activated")

    # 3. Get new prompt (without restart!)
    print("\n3. New CODE_REVIEW prompt (v1.1) - NO RESTART:")
    print("-" * 60)
    prompt_v1_1 = context_mgr._get_system_prompt(TaskType.CODE_REVIEW)
    print(prompt_v1_1[:200] + "...")

    # 4. Rollback to v1.0
    print("\n4. Rolling back to v1.0...")
    manager.register_prompt(
        template_id="system_code_review", version="1.0", content=prompt_v1, is_active=True
    )
    print("✓ Rolled back to v1.0")

    # 5. Verify rollback
    print("\n5. Verify rollback:")
    print("-" * 60)
    prompt_rollback = context_mgr._get_system_prompt(TaskType.CODE_REVIEW)
    print(prompt_rollback[:200] + "...")

    # 6. Show version history
    print("\n6. Version history:")
    print("-" * 60)
    versions = db.fetchall("""
        SELECT version, is_active, effective_at
        FROM prompt_templates
        WHERE template_id = 'system_code_review'
        ORDER BY effective_at DESC
    """)

    for v in versions:
        status = "✓ ACTIVE" if v["is_active"] else "  inactive"
        print(f"  {status} v{v['version']} - {v['effective_at']}")

    print("\n" + "=" * 60)
    print("✓ Demo complete! Prompts can be modified at runtime.")
    print("=" * 60)


if __name__ == "__main__":
    demo_dynamic_prompts()
