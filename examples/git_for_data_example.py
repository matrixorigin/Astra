"""Git for Data example - Time Machine and Sandbox.

Demonstrates snapshot, restore, and sandbox capabilities.
"""

from core.events.event_logger import EventLogger
from core.events.session_manager import SessionManager
from core.replay.time_machine import TimeMachine
from core.sandbox import Sandbox
from sdk import Database

# Initialize
db = Database()
session_mgr = SessionManager(db)
logger = EventLogger(db)
time_machine = TimeMachine(db)
sandbox = Sandbox(db=db)

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
sandbox_name = "sandbox_test"
sandbox.create(sandbox_name)

# Run experiment in sandbox
db.execute(f"INSERT INTO {sandbox_name}.conversation_events (event_id, event_type, user_id, session_id, content, created_at) SELECT event_id, event_type, user_id, session_id, content, created_at FROM dev_agent.conversation_events LIMIT 1")

# Compare
main_count = db.fetchone("SELECT COUNT(*) as count FROM dev_agent.conversation_events")["count"]
sandbox_count = db.fetchone(f"SELECT COUNT(*) as count FROM {sandbox_name}.conversation_events")["count"]
print(f"✓ Sandbox has {sandbox_count} events, main has {main_count} events")
print("✓ Main timeline is preserved (sandbox changes are isolated)")

# Cleanup
sandbox.delete(sandbox_name)
time_machine.git.drop_snapshot("my_checkpoint")
print("\n✓ Cleaned up sandbox and checkpoint")
