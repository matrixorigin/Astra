#!/usr/bin/env python3
"""Demo: Side-Effect Isolation for Safe Replay

This demo shows how the ToolMockingLayer prevents dangerous operations
during replay while allowing safe read operations.
"""

from core.skills.base import (
    AccessScope,
    RepoType,
    SideEffectCategory,
    SideEffectProfile,
    Skill,
    SkillInput,
    SkillOutput,
    SkillRequirement,
)
from core.skills.mocking import MockMode, SecurityError, ToolMockingLayer
from sdk import Database

# ============================================================================
# Demo Skills
# ============================================================================


class GetPRSkill(Skill):
    """READ skill - safe to replay"""

    name = "get_pr"
    version = "1.0.0"
    description = "Get PR details"
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.READ, llm_required=False
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.READ, external_apis=["github"]
    )

    def validate_input(self, input_data: dict) -> SkillInput:
        return SkillInput(**input_data)

    def execute(self, input: SkillInput) -> SkillOutput:
        print("  → Fetching PR from GitHub API...")
        return SkillOutput(
            success=True, result={"number": 123, "title": "Fix bug", "state": "open"}
        )


class MergePRSkill(Skill):
    """DESTRUCTIVE skill - blocked in replay"""

    name = "merge_pr"
    version = "1.0.0"
    description = "Merge a PR"
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.WRITE, llm_required=False
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.DESTRUCTIVE, external_apis=["github"]
    )

    def validate_input(self, input_data: dict) -> SkillInput:
        return SkillInput(**input_data)

    def execute(self, input: SkillInput) -> SkillOutput:
        print("  → Merging PR on GitHub...")
        return SkillOutput(success=True, result={"merged": True})


# ============================================================================
# Demo
# ============================================================================


def main():
    db = Database()
    session_id = "demo_session_replay_safety"

    print("=" * 70)
    print("Side-Effect Isolation Demo")
    print("=" * 70)

    # Cleanup previous demo data
    db.execute("DELETE FROM conversation_events WHERE session_id = %s", (session_id,))

    # ========================================================================
    # Scenario 1: READ operations are safe in replay
    # ========================================================================
    print("\n📖 Scenario 1: READ Operations (Safe in Replay)")
    print("-" * 70)

    read_skill = GetPRSkill()
    params = {"user_id": "alice", "session_id": session_id}

    print("\n1. Production Mode:")
    prod_layer = ToolMockingLayer(MockMode.PRODUCTION, db)
    result1 = prod_layer.execute(read_skill, params, session_id)
    print(f"  ✓ Result: {result1.result}")

    print("\n2. Replay Mode:")
    replay_layer = ToolMockingLayer(MockMode.REPLAY, db)
    result2 = replay_layer.execute(read_skill, params, session_id)
    print(f"  ✓ Result: {result2.result}")
    print("  ✓ READ operations can execute in replay mode (safe)")

    # ========================================================================
    # Scenario 2: DESTRUCTIVE operations are blocked in replay
    # ========================================================================
    print("\n\n💥 Scenario 2: DESTRUCTIVE Operations (Blocked in Replay)")
    print("-" * 70)

    destructive_skill = MergePRSkill()

    print("\n1. Production Mode:")
    result3 = prod_layer.execute(destructive_skill, params, session_id)
    print(f"  ✓ Result: {result3.result}")
    print("  ✓ Merge executed successfully")

    print("\n2. Replay Mode:")
    try:
        replay_layer.execute(destructive_skill, params, session_id)
        print("  ✗ ERROR: Should have been blocked!")
    except SecurityError as e:
        print(f"  ✓ Blocked: {e}")
        print("  ✓ System prevented dangerous replay")

    # ========================================================================
    # Summary
    # ========================================================================
    print("\n" + "=" * 70)
    print("Summary")
    print("=" * 70)
    print("✅ READ operations: Safe to replay (can re-execute)")
    print("✅ WRITE operations: Use recorded results in replay")
    print("✅ DESTRUCTIVE operations: Blocked in replay mode")
    print("\n💡 This ensures Agent replay is always safe!")

    # Cleanup
    db.execute("DELETE FROM conversation_events WHERE session_id = %s", (session_id,))


if __name__ == "__main__":
    main()
