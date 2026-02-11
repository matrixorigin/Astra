"""Demo: Auditable and Self-Improving Skill Selection

This demo showcases the breakthrough features of mo-agent-engine's skill selection:
1. Auditable selection with data snapshots
2. Sandbox pre-validation
3. Automatic learning from failures
4. Regression gate for selector changes
"""

import asyncio
from datetime import datetime

from core.skills.auditable_selector import AuditableSkillSelector
from core.skills.regression_gate import SkillSelectionRegressionGate
from core.skills.self_improving_selector import SelfImprovingSelector
from sdk import Database


class MockLLMClient:
    """Mock LLM client for demo."""

    def chat_with_tools(self, messages, tools, tool_choice="auto"):
        # Mock response
        return {
            "tool_calls": [
                {
                    "function": {
                        "name": "code_review",
                        "arguments": '{"repo_id": "test/repo", "pr_number": 123}',
                    }
                }
            ]
        }


async def demo_auditable_selection():
    """Demo 1: Auditable skill selection with snapshots."""
    print("\n" + "=" * 80)
    print("DEMO 1: Auditable Skill Selection")
    print("=" * 80)

    db = Database()
    llm = MockLLMClient()
    selector = AuditableSkillSelector(db, llm)

    # Scenario: User asks to review a PR
    query = "Review PR #123 for security issues"
    session_id = "demo_session_001"

    print(f"\n📝 Query: {query}")
    print(f"🔍 Session: {session_id}")

    # Select skills with full auditability
    event = selector.select_with_validation(
        query=query, session_id=session_id, validate_in_sandbox=True
    )

    print(f"\n✅ Selection completed:")
    print(f"   Event ID: {event.event_id}")
    print(f"   Snapshot: {event.context_snapshot}")
    print(f"   Selected: {event.selected_skills}")
    print(f"   Method: {event.selection_method}")
    print(f"   Reasoning: {event.selection_reasoning}")

    print(f"\n📊 Candidate scores:")
    for skill, score in event.candidate_scores.items():
        print(f"   {skill}: {score:.2f}")

    # Simulate execution result
    selector.update_execution_result(
        event_id=event.event_id,
        success=True,
        time_ms=1234,
        cost=0.05,
        result={"status": "completed", "issues_found": 3},
    )

    # Simulate user feedback
    selector.update_user_feedback(event_id=event.event_id, score=5)

    print(f"\n✅ Execution result recorded")
    print(f"   Success: True")
    print(f"   Time: 1234ms")
    print(f"   Cost: $0.05")
    print(f"   User feedback: 5/5 ⭐")

    # Time-travel debugging
    print(f"\n🕰️  Time-travel debugging:")
    print(f"   To replay this selection, use:")
    print(f"   SELECT * FROM events {{SNAPSHOT = '{event.context_snapshot}'}}")
    print(f"   This shows the EXACT data state the selector saw")

    return event


async def demo_self_improving():
    """Demo 2: Self-improving selector that learns from failures."""
    print("\n" + "=" * 80)
    print("DEMO 2: Self-Improving Selector")
    print("=" * 80)

    db = Database()
    llm = MockLLMClient()
    learner = SelfImprovingSelector(db, llm)

    # Simulate some historical failures
    print("\n📚 Simulating historical failures...")
    print("   (In production, these come from real user feedback)")

    # Learn from failures
    print("\n🧠 Learning from failures...")
    stats = learner.learn_from_failures(days=7)

    print(f"\n✅ Learning completed:")
    print(f"   Failures analyzed: {stats['failures_analyzed']}")
    print(f"   Corrections found: {stats['corrections_found']}")
    print(f"   Learnings added: {stats['learnings_added']}")

    # Get learning stats
    learning_stats = learner.get_learning_stats()
    print(f"\n📊 Learning statistics:")
    print(f"   Total learnings: {learning_stats['total_learnings']}")
    print(f"   Avg confidence: {learning_stats['avg_confidence']:.2f}")
    print(f"   Total evidence: {learning_stats['total_evidence']}")
    print(f"   Total applications: {learning_stats['total_applications']}")
    print(f"   High confidence: {learning_stats['high_confidence_learnings']}")

    # Apply learnings to new query
    query = "Review PR #456"
    candidates = ["code_review", "summarize_pr", "list_prs"]

    print(f"\n🔄 Applying learnings to new query:")
    print(f"   Query: {query}")
    print(f"   Original candidates: {candidates}")

    corrected = learner.apply_learnings(query, candidates)
    print(f"   Corrected candidates: {corrected}")

    if corrected != candidates:
        print(f"   ✅ Applied learned correction!")
    else:
        print(f"   ℹ️  No corrections needed")


async def demo_regression_gate():
    """Demo 3: Regression gate for selector changes."""
    print("\n" + "=" * 80)
    print("DEMO 3: Regression Gate")
    print("=" * 80)

    db = Database()
    llm = MockLLMClient()
    gate = SkillSelectionRegressionGate(db, llm)

    # Create two selectors (simulating old and new versions)
    old_selector = AuditableSkillSelector(db, llm)
    new_selector = AuditableSkillSelector(db, llm)

    print("\n🚪 Testing new selector against golden queries...")
    print("   (Golden queries = high-quality historical selections)")

    # Run regression gate
    result = gate.validate_selector_change(
        new_selector=new_selector,
        old_selector=old_selector,
        selector_version="v2.1.0",
        min_improvement=-0.05,  # Allow 5% degradation
    )

    print(f"\n✅ Gate result:")
    print(f"   Verdict: {result['verdict']}")
    print(f"   Test queries: {result['test_queries_count']}")
    print(f"   New selector score: {result['new_selector_avg_score']:.2f}")
    print(f"   Old selector score: {result['old_selector_avg_score']:.2f}")
    print(f"   Improvement: {result['improvement_pct']:.1f}%")
    print(f"   Reason: {result['reason']}")

    if result["verdict"] == "PASS":
        print(f"\n   ✅ New selector approved for deployment!")
    else:
        print(f"\n   ❌ New selector rejected - regression detected")

    # Get gate statistics
    stats = gate.get_gate_stats()
    print(f"\n📊 Gate statistics:")
    print(f"   Total gates: {stats['total_gates']}")
    print(f"   Passed: {stats['passed']}")
    print(f"   Failed: {stats['failed']}")
    print(f"   Pass rate: {stats['pass_rate']:.1%}")
    print(f"   Avg improvement: {stats['avg_improvement_pct']:.1f}%")


async def demo_complete_workflow():
    """Demo 4: Complete workflow combining all features."""
    print("\n" + "=" * 80)
    print("DEMO 4: Complete Workflow")
    print("=" * 80)

    db = Database()
    llm = MockLLMClient()

    # Step 1: Auditable selection
    print("\n📝 Step 1: Auditable selection with sandbox validation")
    selector = AuditableSkillSelector(db, llm)
    event = selector.select_with_validation(
        query="Review PR #789 for performance issues",
        session_id="demo_workflow",
        validate_in_sandbox=True,
    )
    print(f"   ✅ Selected: {event.selected_skills}")
    print(f"   📸 Snapshot: {event.context_snapshot}")

    # Step 2: Execute and record result
    print("\n⚙️  Step 2: Execute skill and record result")
    selector.update_execution_result(
        event_id=event.event_id, success=True, time_ms=2000, cost=0.08, result={}
    )
    selector.update_user_feedback(event_id=event.event_id, score=4)
    print(f"   ✅ Execution recorded")
    print(f"   ⭐ User feedback: 4/5")

    # Step 3: Learn from history
    print("\n🧠 Step 3: Learn from historical failures")
    learner = SelfImprovingSelector(db, llm)
    stats = learner.learn_from_failures(days=7)
    print(f"   ✅ Learned from {stats['failures_analyzed']} failures")

    # Step 4: Regression gate
    print("\n🚪 Step 4: Validate selector improvements")
    gate = SkillSelectionRegressionGate(db, llm)
    old_selector = AuditableSkillSelector(db, llm)
    new_selector = AuditableSkillSelector(db, llm)
    result = gate.validate_selector_change(
        new_selector=new_selector, old_selector=old_selector, selector_version="v2.2.0"
    )
    print(f"   {result['verdict']}: {result['reason']}")

    print("\n" + "=" * 80)
    print("✅ Complete workflow demonstrated!")
    print("=" * 80)


async def main():
    """Run all demos."""
    print("\n" + "=" * 80)
    print("🚀 mo-agent-engine: Breakthrough Skill Selection Demo")
    print("=" * 80)
    print("\nThis demo showcases capabilities that NO other agent framework has:")
    print("1. ✅ Auditable selection with data snapshots")
    print("2. ✅ Sandbox pre-validation before execution")
    print("3. ✅ Automatic learning from failures")
    print("4. ✅ Regression gate for selector changes")
    print("\nThese are enabled by Git for Data + Event Sourcing + Sandbox")

    try:
        # Run demos
        await demo_auditable_selection()
        await demo_self_improving()
        await demo_regression_gate()
        await demo_complete_workflow()

        print("\n" + "=" * 80)
        print("🎉 All demos completed successfully!")
        print("=" * 80)
        print("\n💡 Key takeaways:")
        print("   • Every selection is auditable - can replay any decision")
        print("   • Sandbox validation prevents wrong selections")
        print("   • Automatic learning from failures - no manual labeling")
        print("   • Regression gate prevents selector degradation")
        print("\n🌟 This is the future of agent skill selection!")

    except Exception as e:
        print(f"\n❌ Demo failed: {e}")
        import traceback

        traceback.print_exc()


if __name__ == "__main__":
    asyncio.run(main())
