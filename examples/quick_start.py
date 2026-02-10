"""Simple quick start example.

Demonstrates basic usage in 5 minutes.
"""

from core.events.event_logger import EventLogger
from core.events.session_manager import SessionManager
from sdk import Database

# Initialize
db = Database()
session_mgr = SessionManager(db)
logger = EventLogger(db)

# Create a conversation session
session = session_mgr.create_session(user_id="alice")
print(f"Session created: {session.session_id}")

# Log user query
user_event = logger.create_user_query(
    user_id="alice",
    session_id=session.session_id,
    content="What is event sourcing?",
)
print(f"User query logged: {user_event.event_id}")

# Log LLM response
llm_event = logger.create_llm_response(
    user_id="alice",
    session_id=session.session_id,
    content="Event sourcing is a pattern where...",
    agent_id="dev-agent",
    agent_version="0.1.0",
    parent_event_id=user_event.event_id,
    causal_chain_id=user_event.causal_chain_id,
    token_usage={"prompt": 100, "completion": 200, "total": 300},
)
print(f"LLM response logged: {llm_event.event_id}")

# Update session
session_mgr.update_session_activity(session.session_id, llm_event.event_id)

# Check session state
updated = session_mgr.get_session(session.session_id)
print(f"Session has {updated.event_count} events")
