"""Tests for advanced sandbox with CLONE and PITR.

Tests MatrixOne's Git for Data advanced features.
"""

import pytest
from ulid import ULID

from core.events.event_logger import EventLogger
from core.sandbox.advanced_sandbox import AdvancedSandbox
from sdk.database import Database


@pytest.fixture
def db():
    """Database fixture."""
    return Database()


@pytest.fixture
def sandbox(db):
    """Advanced sandbox fixture."""
    return AdvancedSandbox(source_database="dev_agent", db=db)


@pytest.fixture
def event_logger(db):
    """Event logger fixture."""
    return EventLogger(db)


def test_create_clone_sandbox(sandbox):
    """Test creating a sandbox using database clone."""
    sandbox_name = f"test_sandbox_{str(ULID())[:8]}".lower()

    # Create clone sandbox
    sb = sandbox.create_clone_sandbox(sandbox_name)
    assert sb["sandbox_name"] == sandbox_name
    assert sb["sandbox_type"] == "clone"
    assert sb["source_database"] == "dev_agent"

    # Verify sandbox exists
    databases = sandbox.list_databases()
    assert sandbox_name in databases

    # Cleanup
    sandbox.drop_clone_sandbox(sandbox_name)

    # Verify cleanup
    databases = sandbox.list_databases()
    assert sandbox_name not in databases


def test_clone_sandbox_with_data(sandbox, event_logger, db):
    """Test that cloned sandbox contains data from source."""
    sandbox_name = f"test_sandbox_{str(ULID())[:8]}".lower()

    # Create some data in main database
    user_id = f"test_user_{ULID()}"
    session_id = f"test_session_{ULID()}"
    event = event_logger.create_user_query(
        user_id=user_id,
        session_id=session_id,
        content="Test query for clone",
    )

    # Get count in main database
    main_count = db.fetchone(
        "SELECT COUNT(*) as count FROM dev_agent.conversation_events"
    )["count"]

    # Create clone sandbox
    sandbox.create_clone_sandbox(sandbox_name)

    # Verify data exists in sandbox
    sandbox_count = db.fetchone(
        f"SELECT COUNT(*) as count FROM {sandbox_name}.conversation_events"
    )["count"]

    assert sandbox_count == main_count
    assert sandbox_count > 0

    # Verify specific event exists
    result = db.fetchone(
        f"SELECT * FROM {sandbox_name}.conversation_events WHERE event_id = %s",
        (event.event_id,),
    )
    assert result is not None
    assert result["content"] == "Test query for clone"

    # Cleanup
    sandbox.drop_clone_sandbox(sandbox_name)


def test_isolated_experiment(sandbox, event_logger):
    """Test running an isolated experiment in sandbox."""
    experiment_name = "test_experiment"

    def experiment(sandbox_name):
        """Experiment function that modifies sandbox data."""
        # Add data in sandbox
        event_logger.create_user_query(
            user_id="exp_user",
            session_id="exp_session",
            content="Experiment query",
        )
        return {"events_added": 1}

    # Run experiment
    result = sandbox.run_isolated_experiment(
        experiment_name,
        experiment,
        cleanup=True,
    )

    assert result["status"] == "success"
    assert result["result"]["events_added"] == 1

    # Verify sandbox was cleaned up
    databases = sandbox.list_databases()
    assert not any("sandbox_test_experiment" in db for db in databases)


def test_compare_sandbox_with_main(sandbox, db):
    """Test comparing sandbox data with main database."""
    sandbox_name = f"test_sandbox_{str(ULID())[:8]}".lower()

    # Create sandbox (clones current state)
    sandbox.create_clone_sandbox(sandbox_name)
    
    # Get counts after clone
    main_count_before = db.fetchone(
        "SELECT COUNT(*) as count FROM dev_agent.conversation_events"
    )["count"]
    sandbox_count_before = db.fetchone(
        f"SELECT COUNT(*) as count FROM {sandbox_name}.conversation_events"
    )["count"]
    
    # They should be equal initially
    assert main_count_before == sandbox_count_before

    # Add data to sandbox only (direct SQL)
    db.execute(
        f"""
        INSERT INTO {sandbox_name}.conversation_events (
            event_id, user_id, session_id, agent_id, agent_version,
            event_type, content, causal_chain_id
        ) VALUES (%s, %s, %s, %s, %s, %s, %s, %s)
        """,
        ("test_event_id", "sandbox_user", "sandbox_session", "dev-agent", "0.1.0",
         "user_query", "Sandbox only query", "test_chain")
    )

    # Compare
    comparison = sandbox.compare_sandbox_with_main(sandbox_name, "conversation_events")

    assert comparison["table"] == "conversation_events"
    assert comparison["sandbox_count"] == main_count_before + 1
    assert comparison["difference"] == 1  # One more event in sandbox

    # Cleanup
    sandbox.drop_clone_sandbox(sandbox_name)


def test_clone_from_snapshot(sandbox, db):
    """Test creating sandbox from a specific snapshot."""
    # Create a snapshot first
    snapshot_name = f"test_snap_{str(ULID())[:8]}".lower()
    db.execute(f"CREATE SNAPSHOT {snapshot_name} FOR DATABASE dev_agent")

    # Create sandbox from snapshot
    sandbox_name = f"test_sandbox_{str(ULID())[:8]}".lower()
    sb = sandbox.create_clone_sandbox(sandbox_name, from_snapshot=snapshot_name)

    assert sb["from_snapshot"] == snapshot_name
    assert sb["sandbox_name"] == sandbox_name

    # Verify sandbox exists
    databases = sandbox.list_databases()
    assert sandbox_name in databases

    # Cleanup
    sandbox.drop_clone_sandbox(sandbox_name)
    db.execute(f"DROP SNAPSHOT {snapshot_name}")


def test_sandbox_isolation(sandbox, db):
    """Test that sandbox changes don't affect main database."""
    sandbox_name = f"test_sandbox_{str(ULID())[:8]}".lower()

    # Create sandbox
    sandbox.create_clone_sandbox(sandbox_name)
    
    # Get counts after clone
    main_count_before = db.fetchone(
        "SELECT COUNT(*) as count FROM dev_agent.conversation_events"
    )["count"]

    # Add data to sandbox (direct SQL)
    for i in range(5):
        db.execute(
            f"""
            INSERT INTO {sandbox_name}.conversation_events (
                event_id, user_id, session_id, agent_id, agent_version,
                event_type, content, causal_chain_id
            ) VALUES (%s, %s, %s, %s, %s, %s, %s, %s)
            """,
            (f"test_event_{i}", "sandbox_user", "sandbox_session", "dev-agent", "0.1.0",
             "user_query", f"Sandbox query {i}", "test_chain")
        )

    # Verify main database unchanged
    main_count_after = db.fetchone(
        "SELECT COUNT(*) as count FROM dev_agent.conversation_events"
    )["count"]

    assert main_count_after == main_count_before  # Main database unchanged

    # Verify sandbox has new data
    sandbox_count = db.fetchone(
        f"SELECT COUNT(*) as count FROM {sandbox_name}.conversation_events"
    )["count"]

    assert sandbox_count == main_count_before + 5  # Sandbox has 5 more events

    # Cleanup
    sandbox.drop_clone_sandbox(sandbox_name)
