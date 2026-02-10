"""Git for Data example - Time Machine and Sandbox.

Demonstrates snapshot, restore, and sandbox capabilities.
"""

from core.events.event_logger import EventLogger
from core.events.session_manager import SessionManager
from core.replay.time_machine import TimeMachine
from core.sandbox.sandbox import Sandbox
from sdk import Database

# Initialize
db = Database()
session_mgr = SessionManager(db)
logger = EventLogger(db)
time_machine = TimeMachine(db)
sandbox = Sandbox(db)

# Create initial state
session = session_mgr.create_session(user_id="bob")
event1 = logger.create_user_query(
    user_id="bob",
    session_id=session.session_id,
    content="Initial query",
)
print(f"✓ Created initial event: {event1.event_id}")

# Create checkpoint
checkpoint = time_machine.create_checkpoint(
    "my_checkpoint",
    "Before making changes",
)
print(f"✓ Created checkpoint: {checkpoint['checkpoint_name']}")

# Make some changes
event2 = logger.create_user_query(
    user_id="bob",
    session_id=session.session_id,
    content="Another query",
)
print(f"✓ Added new event: {event2.event_id}")

# Restore to checkpoint
print("\n✓ Restoring to checkpoint...")
time_machine.restore_to_checkpoint("my_checkpoint")
print("✓ Restored! Changes after checkpoint are gone.")

# Run experiment in sandbox
print("\n✓ Running experiment in sandbox...")


def experiment():
    """Experiment function."""
    exp_event = logger.create_user_query(
        user_id="bob",
        session_id=session.session_id,
        content="Experimental query",
    )
    return {"event_id": exp_event.event_id}


result = sandbox.run_experiment("test", experiment, cleanup=True)
print(f"✓ Experiment completed: {result['status']}")
print("✓ Main timeline is preserved (sandbox changes are isolated)")

# Cleanup
time_machine.git.drop_snapshot("my_checkpoint")
print("\n✓ Cleaned up checkpoint")
