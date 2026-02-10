"""Tests for P0 priority sandbox features.

Tests table-level operations and sandbox management.
"""

import pytest
from ulid import ULID

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


def test_clone_table_to_sandbox(sandbox, db):
    """Test cloning a specific table to sandbox."""
    sandbox_name = f"test_sandbox_{str(ULID())[:8]}".lower()

    # Create empty sandbox first
    db.execute(f"CREATE DATABASE {sandbox_name}")

    # Clone specific table
    result = sandbox.clone_table_to_sandbox(sandbox_name, "conversation_events")

    assert result["sandbox_name"] == sandbox_name
    assert "conversation_events" in result["target_table"]

    # Verify table exists in sandbox
    tables = sandbox.list_sandbox_tables(sandbox_name)
    assert "conversation_events" in tables

    # Cleanup
    sandbox.drop_clone_sandbox(sandbox_name)


def test_list_sandbox_tables(sandbox):
    """Test listing tables in a sandbox."""
    sandbox_name = f"test_sandbox_{str(ULID())[:8]}".lower()

    # Create sandbox with all tables
    sandbox.create_clone_sandbox(sandbox_name)

    # List tables
    tables = sandbox.list_sandbox_tables(sandbox_name)

    assert len(tables) > 0
    assert "conversation_events" in tables
    assert "sessions" in tables

    # Cleanup
    sandbox.drop_clone_sandbox(sandbox_name)


def test_remove_table_from_sandbox(sandbox):
    """Test removing a table from sandbox."""
    sandbox_name = f"test_sandbox_{str(ULID())[:8]}".lower()

    # Create sandbox
    sandbox.create_clone_sandbox(sandbox_name)

    # Get initial table count
    tables_before = sandbox.list_sandbox_tables(sandbox_name)
    initial_count = len(tables_before)

    # Remove a table
    sandbox.remove_table_from_sandbox(sandbox_name, "sessions")

    # Verify table removed
    tables_after = sandbox.list_sandbox_tables(sandbox_name)
    assert len(tables_after) == initial_count - 1
    assert "sessions" not in tables_after

    # Cleanup
    sandbox.drop_clone_sandbox(sandbox_name)


def test_list_sandboxes(sandbox):
    """Test listing all sandboxes."""
    # Create multiple sandboxes
    sandbox1 = f"sandbox_test_{str(ULID())[:8]}".lower()
    sandbox2 = f"sandbox_test_{str(ULID())[:8]}".lower()

    sandbox.create_clone_sandbox(sandbox1)
    sandbox.create_clone_sandbox(sandbox2)

    # List sandboxes
    sandboxes = sandbox.list_sandboxes(prefix="sandbox_test_")

    sandbox_names = [s["sandbox_name"] for s in sandboxes]
    assert sandbox1 in sandbox_names
    assert sandbox2 in sandbox_names

    # Cleanup
    sandbox.drop_clone_sandbox(sandbox1)
    sandbox.drop_clone_sandbox(sandbox2)


def test_get_sandbox_info(sandbox):
    """Test getting detailed sandbox information."""
    sandbox_name = f"test_sandbox_{str(ULID())[:8]}".lower()

    # Create sandbox
    sandbox.create_clone_sandbox(sandbox_name)

    # Get info
    info = sandbox.get_sandbox_info(sandbox_name)

    assert info["sandbox_name"] == sandbox_name
    assert info["table_count"] > 0
    assert len(info["tables"]) > 0
    assert info["source_database"] == "dev_agent"

    # Verify table info structure
    first_table = info["tables"][0]
    assert "table" in first_table
    assert "row_count" in first_table

    # Cleanup
    sandbox.drop_clone_sandbox(sandbox_name)


def test_sandbox_metadata(sandbox):
    """Test sandbox metadata management."""
    sandbox_name = f"test_sandbox_{str(ULID())[:8]}".lower()

    # Create sandbox
    sandbox.create_clone_sandbox(sandbox_name)

    # Update metadata
    result = sandbox.update_sandbox_metadata(
        sandbox_name,
        description="Test sandbox for experiments",
        tags=["test", "experiment"],
    )

    assert result["description"] == "Test sandbox for experiments"
    assert result["tags"] == ["test", "experiment"]

    # Get metadata
    metadata = sandbox.get_sandbox_metadata(sandbox_name)

    assert metadata["description"] == "Test sandbox for experiments"
    assert metadata["tags"] == ["test", "experiment"]

    # Cleanup
    sandbox.drop_clone_sandbox(sandbox_name)


def test_sandbox_checkpoint(sandbox):
    """Test creating and listing sandbox checkpoints."""
    sandbox_name = f"test_sandbox_{str(ULID())[:8]}".lower()
    checkpoint_name = f"checkpoint_{str(ULID())[:8]}".lower()

    # Create sandbox
    sandbox.create_clone_sandbox(sandbox_name)

    # Create checkpoint
    result = sandbox.create_sandbox_checkpoint(
        sandbox_name, checkpoint_name, "Test checkpoint"
    )

    assert result["checkpoint_name"] == checkpoint_name
    assert result["description"] == "Test checkpoint"

    # List checkpoints
    checkpoints = sandbox.list_sandbox_checkpoints(sandbox_name)

    assert len(checkpoints) >= 1
    checkpoint_names = [c["checkpoint_name"] for c in checkpoints]
    assert checkpoint_name in checkpoint_names

    # Cleanup
    sandbox.drop_clone_sandbox(sandbox_name)
    sandbox.git.drop_snapshot(checkpoint_name)


def test_add_table_to_sandbox(sandbox, db):
    """Test adding a table to existing sandbox."""
    sandbox_name = f"test_sandbox_{str(ULID())[:8]}".lower()

    # Create empty sandbox
    db.execute(f"CREATE DATABASE {sandbox_name}")

    # Add first table
    sandbox.add_table_to_sandbox(sandbox_name, "conversation_events")

    # Verify table added
    tables = sandbox.list_sandbox_tables(sandbox_name)
    assert "conversation_events" in tables
    assert len(tables) == 1

    # Add second table
    sandbox.add_table_to_sandbox(sandbox_name, "sessions")

    # Verify both tables exist
    tables = sandbox.list_sandbox_tables(sandbox_name)
    assert "conversation_events" in tables
    assert "sessions" in tables
    assert len(tables) == 2

    # Cleanup
    sandbox.drop_clone_sandbox(sandbox_name)
