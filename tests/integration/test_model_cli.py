"""Test model CLI commands."""
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
    yield db


def test_model_list_command(runner):
    """Test model list command."""
    result = runner.invoke(cli, ['model', 'list'])
    assert result.exit_code == 0
    assert 'Available Models' in result.output or 'No models available' in result.output


def test_model_list_with_scope(runner):
    """Test listing models with user scope."""
    result = runner.invoke(cli, ['model', 'list', '--user-id', 'alice'])
    assert result.exit_code == 0


# Note: model add/update/remove commands have been moved to mo-admin CLI
# These operations now require admin privileges and should be tested separately
# in test_admin_cli.py


if __name__ == '__main__':
    pytest.main([__file__, '-v'])
