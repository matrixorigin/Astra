"""Complete example demonstrating mo-dev-agent core capabilities.

This example shows:
1. Session management
2. Event logging (conversation flow)
3. Causal chain tracking
4. Git for Data (time machine and sandbox)
"""

from uuid_utils import uuid7

from core.events.causal_chain import CausalChainManager
from core.events.event_logger import EventLogger
from core.events.event_reader import EventReader
from core.events.session_manager import SessionManager
from core.replay.time_machine import TimeMachine
from core.sandbox import Sandbox
from sdk import Database


def print_section(title: str) -> None:
    """Print a section header."""
    print(f"\n{'=' * 60}")
    print(f"  {title}")
    print(f"{'=' * 60}\n")


def main() -> None:
    """Run the complete example."""
    print_section("mo-dev-agent Complete Example")

    # Initialize components
    db = Database()
    session_mgr = SessionManager(db)
    event_logger = EventLogger(db)
    event_reader = EventReader(db)
    chain_mgr = CausalChainManager(db)
    time_machine = TimeMachine(db)
    sandbox = Sandbox(db=db)

    user_id = f"demo_user_{uuid7()}"

    # ========================================================================
    # Part 1: Basic Conversation Flow
    # ========================================================================
    print_section("Part 1: Basic Conversation Flow")

    # Create session
    session = session_mgr.create_session(
        user_id=user_id,
        metadata={"source": "demo", "purpose": "showcase"},
    )
    print(f"✓ Created session: {session.session_id}")

    # User asks a question
    user_event = event_logger.create_user_query(
        user_id=user_id,
        session_id=session.session_id,
        content="How do I implement event sourcing in Python?",
    )
    session_mgr.update_session_activity(session.session_id, user_event.event_id)
    print(f"✓ User query logged: {user_event.event_id}")

    # LLM responds
    llm_event = event_logger.create_llm_response(
        user_id=user_id,
        session_id=session.session_id,
        content="Event sourcing in Python involves storing all changes as events...",
        agent_id="dev-agent",
        agent_version="0.1.0",
        parent_event_id=user_event.event_id,
        causal_chain_id=user_event.causal_chain_id,
        llm_model_used="gpt-4",
        token_usage={"prompt": 150, "completion": 300, "total": 450},
    )
    session_mgr.update_session_activity(session.session_id, llm_event.event_id)
    print(f"✓ LLM response logged: {llm_event.event_id}")

    # ========================================================================
    # Part 2: Causal Chain Analysis
    # ========================================================================
    print_section("Part 2: Causal Chain Analysis")

    chain = chain_mgr.get_chain(user_event.causal_chain_id)
    print(f"✓ Causal chain has {len(chain)} events")

    summary = chain_mgr.get_chain_summary(user_event.causal_chain_id)
    print(f"✓ Total tokens used: {summary['total_tokens']}")
    print(f"✓ Event types: {summary['event_types']}")

    validation = chain_mgr.validate_chain_integrity(user_event.causal_chain_id)
    print(f"✓ Chain integrity: {'Valid' if validation['is_valid'] else 'Invalid'}")

    # ========================================================================
    # Part 3: Multi-turn Conversation
    # ========================================================================
    print_section("Part 3: Multi-turn Conversation")

    # Second turn
    user_event_2 = event_logger.create_user_query(
        user_id=user_id,
        session_id=session.session_id,
        content="Can you show me a code example?",
    )
    session_mgr.update_session_activity(session.session_id, user_event_2.event_id)

    llm_event_2 = event_logger.create_llm_response(
        user_id=user_id,
        session_id=session.session_id,
        content="Here's a simple example:\n\nclass Event:\n    pass\n...",
        agent_id="dev-agent",
        agent_version="0.1.0",
        parent_event_id=user_event_2.event_id,
        causal_chain_id=user_event_2.causal_chain_id,
        llm_model_used="gpt-4",
        token_usage={"prompt": 200, "completion": 400, "total": 600},
    )
    session_mgr.update_session_activity(session.session_id, llm_event_2.event_id)

    # Check session state
    updated_session = session_mgr.get_session(session.session_id)
    print(f"✓ Session now has {updated_session.event_count} events")

    # ========================================================================
    # Part 4: Git for Data - Time Machine
    # ========================================================================
    print_section("Part 4: Git for Data - Time Machine")

    # Create checkpoint
    checkpoint_name = f"demo_checkpoint_{str(uuid7())[:8]}".lower()
    checkpoint = time_machine.create_checkpoint(
        checkpoint_name,
        "Checkpoint after initial conversation",
    )
    print(f"✓ Created checkpoint: {checkpoint_name}")
    print(f"  Timestamp: {checkpoint['timestamp']}")

    # Add more events
    user_event_3 = event_logger.create_user_query(
        user_id=user_id,
        session_id=session.session_id,
        content="What about error handling?",
    )
    session_mgr.update_session_activity(session.session_id, user_event_3.event_id)
    print("✓ Added event after checkpoint")

    # List all checkpoints
    checkpoints = time_machine.list_checkpoints()
    print(f"✓ Total checkpoints available: {len(checkpoints)}")

    # ========================================================================
    # Part 5: Git for Data - Sandbox Experiment
    # ========================================================================
    print_section("Part 5: Git for Data - Sandbox Experiment")

    def experiment():
        """Run an experiment in isolated sandbox."""
        # Create experimental events
        exp_event = event_logger.create_user_query(
            user_id=user_id,
            session_id=session.session_id,
            content="Experimental query in sandbox",
        )
        return {"event_id": exp_event.event_id, "status": "completed"}

    # Run experiment
    result = sandbox.run_experiment(
        "demo_experiment",
        experiment,
        cleanup=True,
    )
    print(f"✓ Experiment status: {result['status']}")
    print(f"  Result: {result.get('result', {})}")

    # Verify main timeline is unchanged
    final_session = session_mgr.get_session(session.session_id)
    print(f"✓ Main timeline preserved: {final_session.event_count} events")

    # ========================================================================
    # Part 6: Query and Analysis
    # ========================================================================
    print_section("Part 6: Query and Analysis")

    # Get all session events
    all_events = event_reader.get_session_events(session.session_id)
    print(f"✓ Retrieved {len(all_events)} events from session")

    # Get user's cross-session events
    user_events = event_reader.get_user_events(user_id, limit=10)
    print(f"✓ User has {len(user_events)} total events")

    # Close session
    session_mgr.close_session(session.session_id)
    closed_session = session_mgr.get_session(session.session_id)
    print(f"✓ Session closed: {closed_session.status}")

    # ========================================================================
    # Summary
    # ========================================================================
    print_section("Summary")
    print("✓ Demonstrated core capabilities:")
    print("  - Session management")
    print("  - Event logging (user queries + LLM responses)")
    print("  - Causal chain tracking and validation")
    print("  - Multi-turn conversations")
    print("  - Git for Data time machine (checkpoints)")
    print("  - Git for Data sandbox (isolated experiments)")
    print("  - Cross-session queries")
    print("\n✓ All operations completed successfully!")

    # Cleanup checkpoint
    time_machine.git.drop_snapshot(checkpoint_name)
    print(f"\n✓ Cleaned up checkpoint: {checkpoint_name}")


if __name__ == "__main__":
    main()
