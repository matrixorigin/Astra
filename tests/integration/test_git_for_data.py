"""Integration tests for Git for Data features.

Tests snapshot, time-travel, and sandbox capabilities.
"""

import pytest
from uuid_utils import uuid7
from uuid import uuid4
from datetime import datetime, timezone

from core.replay.time_machine import TimeMachine
from core.sandbox import Sandbox
from sdk.database import Database
from sdk.git_for_data import GitForData


@pytest.fixture
def db():
    """Database fixture."""
    return Database()


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
    user_id = str(uuid4())
    session_id = str(uuid4())
    checkpoint_name = f"test_checkpoint_{str(uuid7()).replace('-', '_')[:8]}".lower()

    # Create initial event directly
    db.execute(
        """
        INSERT INTO conversation_events 
        (event_id, session_id, user_id, agent_id, agent_version, event_type, content, created_at)
        VALUES (%s, %s, %s, %s, %s, %s, %s, %s)
        """,
        (str(uuid4()), session_id, user_id, "system", "1.0.0", "user_query", "Initial query", datetime.now(timezone.utc))
    )

    # Create checkpoint
    checkpoint = time_machine.create_checkpoint(checkpoint_name, "Test checkpoint")
    assert checkpoint["checkpoint_name"] == checkpoint_name

    # Create another event after checkpoint
    db.execute(
        """
        INSERT INTO conversation_events 
        (event_id, session_id, user_id, agent_id, agent_version, event_type, content, created_at)
        VALUES (%s, %s, %s, %s, %s, %s, %s, %s)
        """,
        (str(uuid4()), session_id, user_id, "system", "1.0.0", "user_query", "Query after checkpoint", datetime.now(timezone.utc))
    )

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
    user_id = str(uuid4())
    session_id = str(uuid4())
    sandbox_name = f"sandbox_{str(uuid7()).replace('-', '_')}".lower()

    # Create initial event directly
    db.execute(
        """
        INSERT INTO conversation_events 
        (event_id, session_id, user_id, agent_id, agent_version, event_type, content, created_at)
        VALUES (%s, %s, %s, %s, %s, %s, %s, %s)
        """,
        (str(uuid4()), session_id, user_id, "system", "1.0.0", "user_query", "Before experiment", datetime.now(timezone.utc))
    )

    # Create sandbox (clones current state)
    sandbox.create(sandbox_name)

    # Add more events to main (after sandbox creation)
    db.execute(
        """
        INSERT INTO conversation_events 
        (event_id, session_id, user_id, agent_id, agent_version, event_type, content, created_at)
        VALUES (%s, %s, %s, %s, %s, %s, %s, %s)
        """,
        (str(uuid4()), session_id, user_id, "system", "1.0.0", "user_query", "After sandbox creation", datetime.now(timezone.utc))
    )

    # Verify isolation: main has more events than sandbox
    current_db = db.fetchone("SELECT DATABASE() as db")["db"]
    main_count = db.fetchone(f"select count(*) as count from {current_db}.conversation_events")["count"]
    sandbox_count = db.fetchone(
        f"select count(*) as count from {sandbox_name}.conversation_events"
    )["count"]

    assert main_count > sandbox_count

    # Cleanup
    sandbox.delete(sandbox_name)


def test_git_for_data_restore(git, db):
    """Test snapshot restore functionality."""
    snapshot_name = f"test_restore_{str(uuid7()).replace('-', '_')[:8]}".lower()
    test_event_id = str(uuid4())

    # Create test event directly
    db.execute(
        """
        INSERT INTO conversation_events 
        (event_id, session_id, user_id, agent_id, agent_version, event_type, content, created_at)
        VALUES (%s, %s, %s, %s, %s, %s, %s, %s)
        """,
        (test_event_id, "test_session", "test_user", "system", "1.0.0", "user_query", "Original content", datetime.now(timezone.utc))
    )

    # Create snapshot
    git.create_snapshot(snapshot_name)

    # Modify event
    db.execute(
        "UPDATE conversation_events SET content = %s WHERE event_id = %s",
        ("Modified content", str(test_event_id)),
    )

    # Verify modification
    result = db.fetchone(
        "SELECT content FROM conversation_events WHERE event_id = %s",
        (str(test_event_id),),
    )
    assert result["content"] == "Modified content"

    # Restore from snapshot
    git.restore_from_snapshot(snapshot_name)

    # Verify restoration
    result = db.fetchone(
        "SELECT content FROM conversation_events WHERE event_id = %s",
        (str(test_event_id),),
    )
    assert result["content"] == "Original content"

    # Cleanup
    git.drop_snapshot(snapshot_name)
