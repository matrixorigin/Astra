"""Integration tests for Git for Data features.

Tests snapshot, time-travel, and sandbox capabilities.
"""

import pytest
from uuid_utils import uuid7
from uuid import uuid4
from datetime import datetime, timezone

from api.database import get_db_session
from core.replay.time_machine import TimeMachine
from core.sandbox import Sandbox
from sdk.git_for_data import GitForData


@pytest.fixture
def db():
    """Database fixture."""
    return next(get_db_session())


@pytest.fixture
def git(db):
    """Git for Data fixture."""
    return GitForData(db)


@pytest.fixture
def time_machine(db):
    """Time machine fixture."""
    return TimeMachine(db)


@pytest.fixture
def sandbox(db):
    """Sandbox fixture."""
    return Sandbox(db=db)


def test_snapshot_creation_and_listing(git):
    """Test creating and listing snapshots."""
    snapshot_name = f"test_snapshot_{str(uuid7()).replace('-', '_')[:8]}".lower()

    # Create snapshot
    snapshot = git.create_snapshot(snapshot_name)
    assert snapshot["snapshot_name"] == snapshot_name

    # List snapshots
    snapshots = git.list_snapshots()
    snapshot_names = [s["snapshot_name"] for s in snapshots]
    assert snapshot_name in snapshot_names

    # Get snapshot info
    info = git.get_snapshot_info(snapshot_name)
    assert info is not None
    assert info["snapshot_name"] == snapshot_name

    # Cleanup
    git.drop_snapshot(snapshot_name)


def test_time_machine_checkpoint(time_machine, db):
    """Test time machine checkpoint and restore."""
    from sqlalchemy import text
    
    user_id = str(uuid4())
    session_id = str(uuid4())
    checkpoint_name = f"test_checkpoint_{str(uuid7()).replace('-', '_')[:8]}".lower()

    # Create initial event directly
    db.execute(
        text("""
        INSERT INTO conversation_events 
        (event_id, session_id, user_id, agent_id, agent_version, event_type, content, created_at)
        VALUES (:event_id, :session_id, :user_id, :agent_id, :agent_version, :event_type, :content, :created_at)
        """),
        {
            "event_id": str(uuid4()),
            "session_id": session_id,
            "user_id": user_id,
            "agent_id": "system",
            "agent_version": "1.0.0",
            "event_type": "user_query",
            "content": "Initial query",
            "created_at": datetime.now(timezone.utc)
        }
    )
    db.commit()

    # Create checkpoint
    checkpoint = time_machine.create_checkpoint(checkpoint_name, "Test checkpoint")
    assert checkpoint["checkpoint_name"] == checkpoint_name

    # Create another event after checkpoint
    db.execute(
        text("""
        INSERT INTO conversation_events 
        (event_id, session_id, user_id, agent_id, agent_version, event_type, content, created_at)
        VALUES (:event_id, :session_id, :user_id, :agent_id, :agent_version, :event_type, :content, :created_at)
        """),
        {
            "event_id": str(uuid4()),
            "session_id": session_id,
            "user_id": user_id,
            "agent_id": "system",
            "agent_version": "1.0.0",
            "event_type": "user_query",
            "content": "Query after checkpoint",
            "created_at": datetime.now(timezone.utc)
        }
    )
    db.commit()

    # List checkpoints
    checkpoints = time_machine.list_checkpoints()
    checkpoint_names = [c["snapshot_name"] for c in checkpoints]
    assert checkpoint_name in checkpoint_names

    # Cleanup
    time_machine.git.drop_snapshot(checkpoint_name)


def test_sandbox_creation(sandbox):
    """Test sandbox creation and deletion."""
    sandbox_name = f"sandbox_{str(uuid7()).replace('-', '_')}".lower()

    # Create sandbox
    sandbox.create(sandbox_name)

    # List sandboxes
    sandboxes = sandbox.list_sandboxes()
    assert any(s["sandbox_name"] == sandbox_name for s in sandboxes)

    # Delete sandbox
    sandbox.delete(sandbox_name)


def test_sandbox_experiment(sandbox, db):
    """Test running an experiment in a sandbox."""
    from sqlalchemy import text
    
    user_id = str(uuid4())
    session_id = str(uuid4())
    sandbox_name = f"sandbox_{str(uuid7()).replace('-', '_')}".lower()

    # Create initial event directly
    db.execute(
        text("""
        INSERT INTO conversation_events 
        (event_id, session_id, user_id, agent_id, agent_version, event_type, content, created_at)
        VALUES (:event_id, :session_id, :user_id, :agent_id, :agent_version, :event_type, :content, :created_at)
        """),
        {
            "event_id": str(uuid4()),
            "session_id": session_id,
            "user_id": user_id,
            "agent_id": "system",
            "agent_version": "1.0.0",
            "event_type": "user_query",
            "content": "Before experiment",
            "created_at": datetime.now(timezone.utc)
        }
    )
    db.commit()

    # Create sandbox (clones current state)
    sandbox.create(sandbox_name)

    # Add more events to main (after sandbox creation)
    db.execute(
        text("""
        INSERT INTO conversation_events 
        (event_id, session_id, user_id, agent_id, agent_version, event_type, content, created_at)
        VALUES (:event_id, :session_id, :user_id, :agent_id, :agent_version, :event_type, :content, :created_at)
        """),
        {
            "event_id": str(uuid4()),
            "session_id": session_id,
            "user_id": user_id,
            "agent_id": "system",
            "agent_version": "1.0.0",
            "event_type": "user_query",
            "content": "After sandbox creation",
            "created_at": datetime.now(timezone.utc)
        }
    )
    db.commit()

    # Verify isolation: main has more events than sandbox
    result = db.execute(text("SELECT DATABASE() as db"))
    current_db = result.first()._mapping["db"]
    
    result = db.execute(text(f"select count(*) as count from {current_db}.conversation_events"))
    main_count = result.first()._mapping["count"]
    
    result = db.execute(text(f"select count(*) as count from {sandbox_name}.conversation_events"))
    sandbox_count = result.first()._mapping["count"]

    assert main_count > sandbox_count

    # Cleanup
    sandbox.delete(sandbox_name)


def test_git_for_data_restore(git, db):
    """Test snapshot restore functionality."""
    from sqlalchemy import text
    
    snapshot_name = f"test_restore_{str(uuid7()).replace('-', '_')[:8]}".lower()
    test_event_id = str(uuid4())

    # Create test event directly
    db.execute(
        text("""
        INSERT INTO conversation_events 
        (event_id, session_id, user_id, agent_id, agent_version, event_type, content, created_at)
        VALUES (:event_id, :session_id, :user_id, :agent_id, :agent_version, :event_type, :content, :created_at)
        """),
        {
            "event_id": test_event_id,
            "session_id": "test_session",
            "user_id": "test_user",
            "agent_id": "system",
            "agent_version": "1.0.0",
            "event_type": "user_query",
            "content": "Original content",
            "created_at": datetime.now(timezone.utc)
        }
    )
    db.commit()

    # Create snapshot
    git.create_snapshot(snapshot_name)

    # Modify event
    db.execute(
        text("UPDATE conversation_events SET content = :content WHERE event_id = :event_id"),
        {"content": "Modified content", "event_id": str(test_event_id)}
    )
    db.commit()

    # Verify modification
    result = db.execute(
        text("SELECT content FROM conversation_events WHERE event_id = :event_id"),
        {"event_id": str(test_event_id)}
    )
    row = result.first()
    assert row._mapping["content"] == "Modified content"

    # Restore from snapshot
    git.restore_from_snapshot(snapshot_name)

    # Verify restoration
    result = db.execute(
        text("SELECT content FROM conversation_events WHERE event_id = :event_id"),
        {"event_id": str(test_event_id)}
    )
    row = result.first()
    assert row._mapping["content"] == "Original content"

    # Cleanup
    git.drop_snapshot(snapshot_name)
