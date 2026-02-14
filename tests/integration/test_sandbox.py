"""Tests for Sandbox."""

import pytest
from uuid_utils import uuid7

from api.database import get_db_session
from core.sandbox import Sandbox


@pytest.fixture
def sandbox(db_session):
    return Sandbox(db=db_session, source_db="test_dev_agent_v3")


def test_create_and_delete(sandbox):
    """Test sandbox creation and deletion."""
    name = f"sandbox_{str(uuid7()).replace('-', '_')}".lower()

    sandbox.create(name, description="Test sandbox")
    sandboxes = sandbox.list_sandboxes()
    assert any(s["sandbox_name"] == name for s in sandboxes)

    sandbox.delete(name)
    sandboxes = sandbox.list_sandboxes()
    assert not any(s["sandbox_name"] == name for s in sandboxes)


def test_list_with_filter(sandbox):
    """Test list with filtering."""
    # Cleanup first just in case
    try:
        sandbox.delete("sandbox_exp_test1")
    except:
        pass
    try:
        sandbox.delete("sandbox_prod_test2")
    except:
        pass

    # Create test sandboxes
    sandbox.create("sandbox_exp_test1")
    sandbox.create("sandbox_prod_test2")

    # List all
    all_sandboxes = sandbox.list_sandboxes()
    assert len(all_sandboxes) >= 2

    # Filter by pattern
    exp_sandboxes = sandbox.list_sandboxes(pattern="%exp%")
    assert any("exp" in s["sandbox_name"] for s in exp_sandboxes)

    # Cleanup
    sandbox.delete("sandbox_exp_test1")
    sandbox.delete("sandbox_prod_test2")


def test_create_from_snapshot(sandbox, db_session):
    """Test creating sandbox from snapshot."""
    from core.git_for_data import GitForData

    git = GitForData(db=db_session)

    # Use full UUID to avoid collisions
    snap_name = f"test_snap_{str(uuid7()).replace('-', '_')}"
    git.create_snapshot(snap_name)

    name = f"sandbox_{str(uuid7()).replace('-', '_')}"
    sandbox.create(name, from_snapshot=snap_name)

    sandboxes = sandbox.list_sandboxes()
    assert any(s["sandbox_name"] == name for s in sandboxes)

    sandbox.delete(name)
    git.drop_snapshot(snap_name)


def test_isolation(sandbox, db_session):
    """Test sandbox isolation."""
    from sqlalchemy import text
    
    name = f"sandbox_{str(uuid7()).replace('-', '_')}".lower()

    # Create table in main and clean up any existing test data
    db_session.execute(text("CREATE TABLE IF NOT EXISTS test_iso (id INT)"))
    db_session.execute(text("DELETE FROM test_iso"))
    db_session.commit()

    # Insert test data
    db_session.execute(text("INSERT INTO test_iso VALUES (1)"))
    db_session.commit()

    # Create sandbox
    sandbox.create(name)
    
    # Clean sandbox data (it clones from main, so may have old data)
    db_session.execute(text(f"DELETE FROM {name}.test_iso"))
    db_session.execute(text(f"INSERT INTO {name}.test_iso VALUES (1)"))  # Start with same data as main
    db_session.commit()

    # Modify sandbox
    db_session.execute(text(f"INSERT INTO {name}.test_iso VALUES (2)"))
    db_session.commit()

    # Verify isolation
    result = db_session.execute(text("SELECT DATABASE() as db"))
    current_db = result.first()._mapping["db"]
    
    result = db_session.execute(text(f"SELECT COUNT(*) as count FROM {current_db}.test_iso"))
    main_count = result.first()._mapping["count"]
    
    result = db_session.execute(text(f"SELECT COUNT(*) as count FROM {name}.test_iso"))
    sandbox_count = result.first()._mapping["count"]

    assert main_count == 1
    assert sandbox_count == 2

    # Cleanup
    sandbox.delete(name)
    db_session.execute(text("DROP TABLE IF EXISTS test_iso"))
    db_session.commit()


def test_clone_table(sandbox, db_session):
    """Test table cloning."""
    from sqlalchemy import text
    
    # Cleanup first
    db_session.commit()
    db_session.execute(text("DROP TABLE IF EXISTS test_clone_src"))
    db_session.execute(text("DROP TABLE IF EXISTS test_clone_dst"))
    db_session.commit()
    
    # Create source table
    db_session.execute(text("CREATE TABLE IF NOT EXISTS test_clone_src (id INT)"))
    db_session.execute(text("INSERT INTO test_clone_src VALUES (1), (2)"))
    db_session.commit()

    # Clone table
    sandbox.clone_table("test_clone_dst", "test_clone_src")

    # Verify
    result = db_session.execute(text("SELECT COUNT(*) as count FROM test_clone_dst"))
    count = result.first()._mapping["count"]
    assert count == 2

    # Cleanup
    db_session.commit()
    db_session.execute(text("DROP TABLE IF EXISTS test_clone_src"))
    db_session.execute(text("DROP TABLE IF EXISTS test_clone_dst"))
    db_session.commit()


def test_add_remove_table(sandbox, db_session):
    """Test add/remove table."""
    from sqlalchemy import text
    
    name = f"sandbox_{str(uuid7()).replace('-', '_')}".lower()

    # Create empty sandbox (no tables)
    db_session.commit()
    db_session.execute(text(f"CREATE DATABASE {name}"))

    # Add table
    sandbox.add_table(name, "conversation_events")
    tables = sandbox.list_tables(name)
    assert "conversation_events" in tables

    # Remove table
    sandbox.remove_table(name, "conversation_events")
    tables = sandbox.list_tables(name)
    assert "conversation_events" not in tables

    # Cleanup
    sandbox.delete(name)


def test_sandbox_info(sandbox, db_session):
    """Test sandbox info."""
    name = f"sandbox_{str(uuid7()).replace('-', '_')}".lower()

    sandbox.create(name, description="Test info")
    info = sandbox.info(name)

    assert info["sandbox_name"] == name
    assert info["table_count"] > 0
    assert len(info["table_details"]) > 0

    sandbox.delete(name)


def test_sandbox_snapshot(sandbox, db_session):
    """Test sandbox snapshot and restore."""
    from sqlalchemy import text
    
    name = f"sandbox_{str(uuid7()).replace('-', '_')}".lower()

    # Create sandbox
    sandbox.create(name, description="Test snapshot")

    # Create snapshot
    sandbox.snapshot(name, "snap1")

    # List snapshots
    snapshots = sandbox.list_snapshots(name)
    assert len(snapshots) > 0
    assert any(s["name"] == "snap1" for s in snapshots)

    # Modify sandbox
    db_session.commit()
    db_session.execute(text(f"DROP TABLE IF EXISTS {name}.conversation_events"))

    # Restore
    sandbox.restore(name, "snap1")

    # Verify restored
    tables = sandbox.list_tables(name)
    assert "conversation_events" in tables

    # Delete sandbox (should also delete snapshots)
    sandbox.delete(name)

    # Verify snapshots are deleted
    all_snapshots = sandbox.git.list_snapshots()
    assert not any(s["snapshot_name"].startswith(f"{name}_") for s in all_snapshots)


def test_use_sandbox(sandbox, db_session):
    """Test switching to sandbox."""
    from sqlalchemy import text
    
    name = f"sandbox_{str(uuid7()).replace('-', '_')}".lower()

    # Insert test data directly
    from uuid import uuid4
    from datetime import datetime, timezone
    
    db_session.execute(
        text("""
        INSERT INTO conversation_events 
        (event_id, session_id, user_id, agent_id, agent_version, event_type, content, causal_chain_id, created_at)
        VALUES (:event_id, :session_id, :user_id, :agent_id, :agent_version, :event_type, :content, :causal_chain_id, :created_at)
        """),
        {
            "event_id": str(uuid4()),
            "session_id": "test_session",
            "user_id": "test",
            "agent_id": "system",
            "agent_version": "1.0.0",
            "event_type": "user_query",
            "content": "test",
            "causal_chain_id": str(uuid4()),
            "created_at": datetime.now(timezone.utc)
        }
    )
    db_session.commit()

    # Create sandbox (clones current data)
    sandbox.create(name)

    # Get current database name
    result = db_session.execute(text("SELECT DATABASE()"))
    current_db = result.scalar()

    # Switch to sandbox
    sandbox.use(name)

    # Query in sandbox (no need to prefix with sandbox name)
    result = db_session.execute(text("SELECT COUNT(*) as count FROM conversation_events"))
    count = result.first()._mapping["count"]
    assert count > 0

    # Switch back to main
    sandbox.use(current_db)

    # Cleanup
    sandbox.delete(name)
