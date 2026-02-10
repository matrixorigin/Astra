"""Integration tests for Git for Data features.

Tests snapshot, time-travel, and sandbox capabilities.
"""

import pytest
from ulid import ULID

from core.events.event_logger import EventLogger
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
def event_logger(db):
    """Event logger fixture."""
    return EventLogger(db)


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
    snapshot_name = f"test_snapshot_{str(ULID())[:8]}".lower()

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


def test_time_machine_checkpoint(time_machine, event_logger):
    """Test time machine checkpoint and restore."""
    user_id = f"test_user_{ULID()}"
    session_id = f"test_session_{ULID()}"
    checkpoint_name = f"test_checkpoint_{str(ULID())[:8]}".lower()

    # Create initial event
    event1 = event_logger.create_user_query(
        user_id=user_id,
        session_id=session_id,
        content="Initial query",
    )

    # Create checkpoint
    checkpoint = time_machine.create_checkpoint(
        checkpoint_name, "Test checkpoint"
    )
    assert checkpoint["checkpoint_name"] == checkpoint_name

    # Create another event after checkpoint
    event2 = event_logger.create_user_query(
        user_id=user_id,
        session_id=session_id,
        content="Query after checkpoint",
    )

    # List checkpoints
    checkpoints = time_machine.list_checkpoints()
    checkpoint_names = [c["snapshot_name"] for c in checkpoints]
    assert checkpoint_name in checkpoint_names

    # Cleanup
    time_machine.git.drop_snapshot(checkpoint_name)


def test_sandbox_creation(sandbox):
    """Test sandbox creation and deletion."""
    sandbox_name = f"sandbox_{str(ULID())[:8]}".lower()

    # Create sandbox
    sandbox.create(sandbox_name)

    # List sandboxes
    sandboxes = sandbox.list()
    assert any(s["sandbox_name"] == sandbox_name for s in sandboxes)

    # Delete sandbox
    sandbox.delete(sandbox_name)


def test_sandbox_experiment(sandbox, event_logger, db):
    """Test running an experiment in a sandbox."""
    user_id = f"test_user_{ULID()}"
    session_id = f"test_session_{ULID()}"
    sandbox_name = f"sandbox_{str(ULID())[:8]}".lower()

    # Create initial event
    initial_event = event_logger.create_user_query(
        user_id=user_id,
        session_id=session_id,
        content="Before experiment",
    )

    # Create sandbox (clones current state)
    sandbox.create(sandbox_name)

    # Add more events to main (after sandbox creation)
    event_logger.create_user_query(
        user_id=user_id,
        session_id=session_id,
        content="After sandbox creation",
    )

    # Verify isolation: main has more events than sandbox
    main_count = db.fetchone("select count(*) as count from dev_agent.conversation_events")["count"]
    sandbox_count = db.fetchone(f"select count(*) as count from {sandbox_name}.conversation_events")["count"]
    
    assert main_count > sandbox_count

    # Cleanup
    sandbox.delete(sandbox_name)


def test_git_for_data_restore(git, event_logger, db):
    """Test snapshot restore functionality."""
    snapshot_name = f"test_restore_{str(ULID())[:8]}".lower()
    test_event_id = f"test_event_{ULID()}"

    # Create test event
    event = event_logger.create_user_query(
        user_id="test_user",
        session_id="test_session",
        content="Original content",
    )
    
    # Update event ID for testing
    db.execute(
        "UPDATE conversation_events SET event_id = %s WHERE event_id = %s",
        (test_event_id, event.event_id),
    )

    # Create snapshot
    git.create_snapshot(snapshot_name)

    # Modify event
    db.execute(
        "UPDATE conversation_events SET content = %s WHERE event_id = %s",
        ("Modified content", test_event_id),
    )

    # Verify modification
    result = db.fetchone(
        "SELECT content FROM conversation_events WHERE event_id = %s",
        (test_event_id,),
    )
    assert result["content"] == "Modified content"

    # Restore from snapshot
    git.restore_from_snapshot(snapshot_name)

    # Verify restoration
    result = db.fetchone(
        "SELECT content FROM conversation_events WHERE event_id = %s",
        (test_event_id,),
    )
    assert result["content"] == "Original content"

    # Cleanup
    git.drop_snapshot(snapshot_name)
