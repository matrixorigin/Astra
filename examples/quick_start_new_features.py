"""Quick start example demonstrating new features.

Shows how to use:
1. Context Snapshots
2. Hallucination Firewall
3. Multi-Signal Relevance Scorer
4. Hierarchical Planning
"""

import asyncio

from core.agent.chat_loop import ChatLoop
from core.agent.executor import AgentExecutor
from core.agent.planner import Planner
from core.agent.selector import AgentSkillSelector
from core.context.manager import ContextManager, TaskType
from core.events.event_logger import EventLogger
from core.llm.client import LLMClient
from core.skills.registry import SkillRegistry
from core.verification.firewall import HallucinationFirewall
from sdk import Database


async def main():
    """Quick start example."""
    print("🚀 mo-agent-engine Quick Start\n")

    # 1. Initialize components
    print("1️⃣ Initializing components...")
    db = Database()
    event_logger = EventLogger(db)
    llm_client = LLMClient(db)
    skill_registry = SkillRegistry(db)

    # 2. Initialize new features
    print("2️⃣ Initializing new features...")

    # Context Manager (with snapshots)
    context_manager = ContextManager(db, embedding_provider="mock")
    print("   ✅ Context Manager (snapshots enabled)")

    # Hallucination Firewall
    firewall = HallucinationFirewall(db, context_manager, threshold=0.7)
    print("   ✅ Hallucination Firewall (threshold=0.7)")

    # Planner
    planner = Planner(llm_client)
    print("   ✅ Hierarchical Planner")

    # 3. Create ChatLoop with all features
    print("\n3️⃣ Creating ChatLoop with all features...")
    selector = AgentSkillSelector(db, llm_client)
    executor = AgentExecutor(db, skill_registry)

    chat_loop = ChatLoop(
        selector=selector,
        executor=executor,
        llm_client=llm_client,
        event_logger=event_logger,
        context_manager=context_manager,
        firewall=firewall,
    )
    print("   ✅ ChatLoop initialized")

    # 4. Example: Simple query with context snapshot
    print("\n4️⃣ Example: Simple query with context snapshot")
    session_id = "demo_session_001"
    user_id = "demo_user"

    query = "Hello, what can you help me with?"
    print(f"   Query: {query}")

    try:
        response = await chat_loop.run_step(
            user_input=query,
            session_id=session_id,
            user_id=user_id,
        )
        print(f"   Response: {response[:100]}...")
        print("   ✅ Context snapshot saved automatically")
    except Exception as e:
        print(f"   ⚠️ Error: {e}")

    # 5. Example: Query context snapshots
    print("\n5️⃣ Example: Query context snapshots")
    snapshots = db.fetchall(
        """
        SELECT snapshot_id, session_id, total_tokens, task_type, created_at
        FROM context_snapshots
        WHERE session_id = %s
        ORDER BY created_at DESC
        LIMIT 5
        """,
        (session_id,),
    )

    if snapshots:
        print(f"   Found {len(snapshots)} snapshots:")
        for snap in snapshots:
            print(
                f"   - {snap['snapshot_id'][:8]}... "
                f"({snap['total_tokens']} tokens, {snap['task_type']})"
            )
    else:
        print("   No snapshots found")

    # 6. Example: Load and inspect snapshot
    if snapshots:
        print("\n6️⃣ Example: Load and inspect snapshot")
        snapshot_id = snapshots[0]["snapshot_id"]
        print(f"   Loading snapshot: {snapshot_id[:8]}...")

        try:
            context = context_manager.load_snapshot(snapshot_id)
            print(f"   ✅ Loaded context:")
            print(f"      - Total tokens: {context.total_tokens}")
            print(f"      - Selected events: {len(context.selected_events)}")
            print(f"      - Task type: {context.task_type}")
            print(f"      - Assembly time: {context.assembly_time_ms}ms")
        except Exception as e:
            print(f"   ⚠️ Error loading snapshot: {e}")

    # 7. Example: Firewall verification
    print("\n7️⃣ Example: Firewall verification")
    test_response = "The system has 42 active users and processed 1000 requests."

    if snapshots:
        snapshot_id = snapshots[0]["snapshot_id"]
        result = firewall.verify_response(test_response, snapshot_id, mode="warn")

        print(f"   Response: {test_response}")
        print(f"   ✅ Verification result:")
        print(f"      - Safe to deliver: {result.safe_to_deliver}")
        print(f"      - Confidence: {result.confidence_score:.2%}")
        print(f"      - Claims verified: {result.claims_verified}")
        print(f"      - Claims failed: {result.claims_failed}")

    # 8. Example: Hierarchical planning
    print("\n8️⃣ Example: Hierarchical planning")
    goal = "Create a simple web service with database and tests"
    print(f"   Goal: {goal}")

    try:
        plan = await planner.create_plan(goal=goal, context="Python FastAPI project")
        print(f"   ✅ Plan created:")
        print(f"      - Plan ID: {plan.plan_id}")
        print(f"      - Steps: {len(plan.steps)}")
        print(f"      - Depth: {plan.depth}")

        for i, step in enumerate(plan.steps[:3], 1):
            print(f"      {i}. {step.description}")
        if len(plan.steps) > 3:
            print(f"      ... and {len(plan.steps) - 3} more steps")
    except Exception as e:
        print(f"   ⚠️ Error creating plan: {e}")

    # 9. Summary
    print("\n" + "=" * 60)
    print("✅ Quick Start Complete!")
    print("=" * 60)
    print("\nNew features demonstrated:")
    print("  ✅ Context Snapshots - Time-travel debugging")
    print("  ✅ Hallucination Firewall - Claim verification")
    print("  ✅ Multi-Signal Relevance Scorer - Smart context selection")
    print("  ✅ Hierarchical Planning - Complex task decomposition")
    print("\nNext steps:")
    print("  1. Try: mo-agent chat --user-id your_name")
    print("  2. Explore: mo-agent session list")
    print("  3. Debug: Load snapshots to see what LLM saw")
    print("  4. Verify: Check firewall logs for claim verification")


if __name__ == "__main__":
    asyncio.run(main())
