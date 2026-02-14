"""Example: Multi-agent code review with fan-out/fan-in.

This example demonstrates how to use the delegation system for parallel
multi-agent collaboration.
"""

import asyncio
from core.skills.delegation import DelegateTaskSkill, DelegateTaskInput
from core.events.models import StreamEventType


async def code_review_example():
    """Example: Parallel code review by multiple specialist agents."""
    
    # Setup (in real usage, these come from AgentRegistry and ChatLoop factory)
    from unittest.mock import MagicMock
    from core.events.models import StreamEvent
    
    # Mock agent registry
    registry = MagicMock()
    agents = {
        "code_agent": MagicMock(system_prompt="You review code quality"),
        "security_agent": MagicMock(system_prompt="You review security"),
        "performance_agent": MagicMock(system_prompt="You review performance"),
    }
    registry.get = lambda agent_id: agents.get(agent_id)
    
    # Mock chat loop factory
    async def mock_stream(*args, **kwargs):
        agent_id = kwargs.get("context", {}).get("agent_id", "unknown")
        await asyncio.sleep(0.1)  # Simulate work
        
        reviews = {
            "code_agent": "Code structure is clean. Consider extracting helper functions.",
            "security_agent": "No SQL injection risks. Add input validation for user data.",
            "performance_agent": "Algorithm is O(n). Consider caching for repeated queries.",
        }
        
        yield StreamEvent(
            event_type=StreamEventType.RUN_STARTED,
            data={},
            agent_id=agent_id,
        )
        
        yield StreamEvent(
            event_type=StreamEventType.TEXT_DELTA,
            data={"text": reviews.get(agent_id, "Review complete")},
            agent_id=agent_id,
        )
        
        yield StreamEvent(
            event_type=StreamEventType.TEXT_DONE,
            data={"text": reviews.get(agent_id, "Review complete")},
            agent_id=agent_id,
        )
        
        yield StreamEvent(
            event_type=StreamEventType.RUN_FINISHED,
            data={},
            agent_id=agent_id,
        )
    
    def factory(system_prompt, agent_id):
        loop = MagicMock()
        loop.run_step_stream = mock_stream
        return loop
    
    # Create delegation skill
    skill = DelegateTaskSkill(registry, factory)
    
    # Fan-out: Delegate to multiple agents in parallel
    print("🚀 Starting parallel code review...")
    print()
    
    inputs = [
        DelegateTaskInput(
            agent_id="code_agent",
            task="Review code quality of auth.py",
            session_id="review_session",
            user_id="developer",
        ),
        DelegateTaskInput(
            agent_id="security_agent",
            task="Review security of auth.py",
            session_id="review_session",
            user_id="developer",
        ),
        DelegateTaskInput(
            agent_id="performance_agent",
            task="Review performance of auth.py",
            session_id="review_session",
            user_id="developer",
        ),
    ]
    
    # Stream multiplexed events
    async for event in skill.execute_parallel_stream(inputs):
        if event.event_type == StreamEventType.AGENT_DELEGATED:
            print(f"[{event.agent_id}] Starting review...")
        
        elif event.event_type == StreamEventType.TEXT_DELTA:
            print(f"[{event.agent_id}] {event.data['text']}")
        
        elif event.event_type == StreamEventType.AGENT_COMPLETED:
            print(f"[{event.agent_id}] ✅ Complete")
            print()
        
        elif event.event_type == StreamEventType.AGENT_PROGRESS:
            # Fan-in: Aggregated results
            agg = event.data["aggregated_results"]
            print("=" * 60)
            print("📊 AGGREGATED RESULTS")
            print("=" * 60)
            print(f"Total agents: {agg['total']}")
            print(f"Successful: {agg['successful']}")
            print(f"Failed: {agg['failed']}")
            print()
            
            for delegation in agg["delegations"]:
                status = "✅" if delegation["success"] else "❌"
                print(f"{status} {delegation['agent_id']}")
                print(f"   {delegation['result']}")
                print()


async def pipeline_example():
    """Example: Sequential delegation (pipeline pattern)."""
    
    from unittest.mock import MagicMock
    from core.events.models import StreamEvent
    
    # Mock setup
    registry = MagicMock()
    agents = {
        "analyzer": MagicMock(system_prompt="Analyze requirements"),
        "implementer": MagicMock(system_prompt="Implement solution"),
        "tester": MagicMock(system_prompt="Test implementation"),
    }
    registry.get = lambda agent_id: agents.get(agent_id)
    
    results = {}
    
    async def mock_run_step(*args, **kwargs):
        agent_id = kwargs.get("context", {}).get("agent_id", "unknown")
        task = kwargs.get("user_input", "")
        
        await asyncio.sleep(0.1)
        
        if agent_id == "analyzer":
            return "Requirements: Need login function with JWT auth"
        elif agent_id == "implementer":
            return "Implemented: login(username, password) -> JWT token"
        elif agent_id == "tester":
            return "Tests: All 5 test cases passed"
        
        return f"Result from {agent_id}"
    
    def factory(system_prompt, agent_id):
        loop = MagicMock()
        loop.run_step = mock_run_step
        return loop
    
    skill = DelegateTaskSkill(registry, factory)
    
    print("🔄 Pipeline: Analyze → Implement → Test")
    print()
    
    # Step 1: Analyze
    print("[analyzer] Analyzing requirements...")
    result1 = await skill.execute(DelegateTaskInput(
        agent_id="analyzer",
        task="Analyze: Need user authentication",
        session_id="pipeline",
        user_id="dev",
    ))
    print(f"[analyzer] {result1.result}")
    print()
    
    # Step 2: Implement based on analysis
    print("[implementer] Implementing solution...")
    result2 = await skill.execute(DelegateTaskInput(
        agent_id="implementer",
        task=f"Implement based on: {result1.result}",
        session_id="pipeline",
        user_id="dev",
    ))
    print(f"[implementer] {result2.result}")
    print()
    
    # Step 3: Test implementation
    print("[tester] Testing implementation...")
    result3 = await skill.execute(DelegateTaskInput(
        agent_id="tester",
        task=f"Test: {result2.result}",
        session_id="pipeline",
        user_id="dev",
    ))
    print(f"[tester] {result3.result}")
    print()
    
    print("✅ Pipeline complete!")


if __name__ == "__main__":
    print("=" * 60)
    print("EXAMPLE 1: Fan-out/Fan-in (Parallel Code Review)")
    print("=" * 60)
    print()
    asyncio.run(code_review_example())
    
    print()
    print("=" * 60)
    print("EXAMPLE 2: Pipeline (Sequential Delegation)")
    print("=" * 60)
    print()
    asyncio.run(pipeline_example())
