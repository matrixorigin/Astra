"""Test model CLI commands."""
import json
import pytest
from click.testing import CliRunner
from cli.mo_agent import cli
from sdk import Database


@pytest.fixture
def runner():
    return CliRunner()


@pytest.fixture
def test_db():
    """Create a test database instance."""
    db = Database()
    # Clean up any test data
    try:
        db.execute("DELETE FROM configs WHERE key_name = 'model_registry' AND scope_type = 'global'")
    except:
        pass
    yield db
    # Cleanup after test
    try:
        db.execute("DELETE FROM configs WHERE key_name = 'model_registry' AND scope_type = 'global'")
    except:
        pass


def test_model_list_command(runner):
    """Test model list command."""
    result = runner.invoke(cli, ['model', 'list'])
    assert result.exit_code == 0
    assert 'Available Models' in result.output or 'No models available' in result.output


def test_model_add_command_validation(runner):
    """Test model add command validation."""
    # Missing scope-id for user scope
    result = runner.invoke(cli, [
        'model', 'add', 'test-model', 'openai',
        '--scope', 'user'
    ])
    assert result.exit_code == 0
    assert '--scope-id is required' in result.output


def test_model_add_and_remove(runner, test_db):
    """Test adding and removing a model."""
    # Add model
    result = runner.invoke(cli, [
        'model', 'add', 'test-gpt-4', 'openai',
        '--context-window', '128000',
        '--price-prompt', '0.01',
        '--price-completion', '0.03',
        '--scope', 'global'
    ])
    assert result.exit_code == 0
    assert 'added successfully' in result.output
    
    # Verify model was added
    row = test_db.fetchone(
        "SELECT value FROM configs WHERE key_name = 'model_registry' AND scope_type = 'global'"
    )
    if row:
        models = json.loads(row['value'])
        model_names = [m['model_name'] for m in models]
        assert 'test-gpt-4' in model_names
    
    # Remove model with force flag
    result = runner.invoke(cli, [
        'model', 'remove', 'test-gpt-4',
        '--scope', 'global',
        '--force'
    ])
    assert result.exit_code == 0
    assert 'removed successfully' in result.output or 'registry now empty' in result.output


def test_model_add_with_fallback(runner, test_db):
    """Test adding a model with fallback."""
    result = runner.invoke(cli, [
        'model', 'add', 'test-expensive', 'openai',
        '--fallback', 'gpt-4o-mini',
        '--scope', 'global'
    ])
    assert result.exit_code == 0
    assert 'added successfully' in result.output
    
    # Cleanup
    runner.invoke(cli, ['model', 'remove', 'test-expensive', '--scope', 'global'])


def test_model_remove_nonexistent(runner):
    """Test removing a model that doesn't exist."""
    result = runner.invoke(cli, [
        'model', 'remove', 'nonexistent-model-xyz',
        '--scope', 'global'
    ])
    assert result.exit_code == 0
    assert 'not found' in result.output or 'No model registry' in result.output


def test_model_list_with_scope(runner):
    """Test listing models with user scope."""
    result = runner.invoke(cli, ['model', 'list', '--user-id', 'alice'])
    assert result.exit_code == 0


def test_model_add_with_tags(runner, test_db):
    """Test adding a model with tags."""
    result = runner.invoke(cli, [
        'model', 'add', 'test-fast-model', 'openai',
        '--tags', 'fast,cheap',
        '--scope', 'global'
    ])
    assert result.exit_code == 0
    assert 'added successfully' in result.output
    
    # Cleanup
    runner.invoke(cli, ['model', 'remove', 'test-fast-model', '--scope', 'global', '--force'])


def test_model_add_validation(runner):
    """Test model add validation."""
    # Invalid model name
    result = runner.invoke(cli, ['model', 'add', 'ab', 'openai'])
    assert result.exit_code == 0
    assert 'Invalid model name' in result.output


def test_model_remove_with_dependency(runner, test_db):
    """Test removing a model that has dependents."""
    # Add primary model
    runner.invoke(cli, [
        'model', 'add', 'primary-model', 'openai',
        '--scope', 'global'
    ])
    
    # Add dependent model
    runner.invoke(cli, [
        'model', 'add', 'dependent-model', 'openai',
        '--fallback', 'primary-model',
        '--scope', 'global'
    ])
    
    # Try to remove primary without force
    result = runner.invoke(cli, [
        'model', 'remove', 'primary-model',
        '--scope', 'global'
    ], input='n\n')
    assert result.exit_code == 0
    assert 'dependent-model' in result.output or 'fallback' in result.output
    
    # Cleanup
    runner.invoke(cli, ['model', 'remove', 'dependent-model', '--scope', 'global', '--force'])
    runner.invoke(cli, ['model', 'remove', 'primary-model', '--scope', 'global', '--force'])


def test_model_show(runner):
    """Test showing model details."""
    result = runner.invoke(cli, ['model', 'show', 'gpt-4'])
    assert result.exit_code == 0
    # Should show model details or not found
    assert 'Model:' in result.output or 'not found' in result.output


def test_model_update(runner, test_db):
    """Test updating model configuration."""
    # Add model first
    runner.invoke(cli, [
        'model', 'add', 'test-update-model', 'openai',
        '--price-prompt', '0.01',
        '--scope', 'global'
    ])
    
    # Update price
    result = runner.invoke(cli, [
        'model', 'update', 'test-update-model',
        '--price-prompt', '0.02',
        '--scope', 'global'
    ])
    assert result.exit_code == 0
    assert 'updated successfully' in result.output
    
    # Cleanup
    runner.invoke(cli, ['model', 'remove', 'test-update-model', '--scope', 'global', '--force'])


if __name__ == '__main__':
    pytest.main([__file__, '-v'])
