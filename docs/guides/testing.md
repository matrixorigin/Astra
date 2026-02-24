# Testing Guide

Complete guide for running and writing tests in mo-agent.

## Quick Start

```bash
# Run all tests (keeps services running)
make dev-test-keep

# Run all tests (stops services after)
make dev-test

# Run unit tests only
make dev-test-unit

# Run integration tests only
make dev-test-integration
```

## Test Structure

```
tests/
├── unit/                    # Unit tests (fast, no external dependencies)
│   ├── test_auth.py
│   ├── test_models.py
│   └── test_utils.py
│
├── integration/             # Integration tests (require database, Redis)
│   ├── test_api.py
│   ├── test_database.py
│   └── test_workflows.py
│
├── conftest.py             # Pytest fixtures
└── __init__.py
```

## Running Tests

### All Tests

```bash
# Keep services running (recommended for development)
make dev-test-keep

# Stop services after tests
make dev-test

# With coverage
make dev-test ARGS="--cov=core --cov-report=html"
```

### Specific Test Categories

```bash
# Unit tests only (fast)
make dev-test-unit

# Integration tests only
make dev-test-integration

# Specific test file
pytest tests/unit/test_auth.py

# Specific test function
pytest tests/unit/test_auth.py::test_login
```

### Test Selection

```bash
# Run tests matching pattern
pytest -k "auth"

# Run tests with specific marker
pytest -m "slow"

# Run failed tests from last run
pytest --lf

# Run failed tests first, then others
pytest --ff
```

### Verbose Output

```bash
# Show all test names
pytest -v

# Show print statements
pytest -s

# Show local variables on failure
pytest -l

# Detailed output
pytest -vv
```

## Test Configuration

### pytest.ini

```ini
[pytest]
testpaths = tests
python_files = test_*.py
python_classes = Test*
python_functions = test_*
markers =
    unit: Unit tests
    integration: Integration tests
    slow: Slow tests
    skip: Skip test
addopts =
    --strict-markers
    --tb=short
    --disable-warnings
```

### Markers

```python
import pytest

# Mark as unit test
@pytest.mark.unit
def test_something():
    pass

# Mark as integration test
@pytest.mark.integration
def test_database():
    pass

# Mark as slow test
@pytest.mark.slow
def test_long_running():
    pass

# Skip test
@pytest.mark.skip(reason="Not implemented yet")
def test_future_feature():
    pass

# Skip conditionally
@pytest.mark.skipif(sys.version_info < (3, 11), reason="Requires Python 3.11+")
def test_new_feature():
    pass
```

## Writing Tests

### Unit Tests

Unit tests should be fast and have no external dependencies.

```python
# tests/unit/test_auth.py
import pytest
from core.auth import hash_password, verify_password

def test_hash_password():
    """Test password hashing."""
    password = "secret123"
    hashed = hash_password(password)
    
    assert hashed != password
    assert len(hashed) > 0

def test_verify_password():
    """Test password verification."""
    password = "secret123"
    hashed = hash_password(password)
    
    assert verify_password(password, hashed)
    assert not verify_password("wrong", hashed)

@pytest.mark.parametrize("password,expected", [
    ("short", False),
    ("longenough", True),
    ("with spaces", True),
])
def test_password_validation(password, expected):
    """Test password validation with multiple inputs."""
    result = validate_password(password)
    assert result == expected
```

### Integration Tests

Integration tests require database and Redis.

```python
# tests/integration/test_api.py
import pytest
from fastapi.testclient import TestClient
from api.main import app

@pytest.fixture
def client():
    """Create test client."""
    return TestClient(app)

@pytest.fixture
def auth_headers(client):
    """Get authentication headers."""
    # Register user
    response = client.post("/auth/register", json={
        "username": "testuser",
        "password": "secret123",
        "email": "test@example.com"
    })
    
    # Login
    response = client.post("/auth/login", json={
        "username": "testuser",
        "password": "secret123"
    })
    token = response.json()["access_token"]
    
    return {"Authorization": f"Bearer {token}"}

def test_create_agent(client, auth_headers):
    """Test agent creation."""
    response = client.post("/agents", 
        headers=auth_headers,
        json={
            "name": "Test Agent",
            "description": "Test description"
        }
    )
    
    assert response.status_code == 200
    data = response.json()
    assert data["name"] == "Test Agent"
    assert "agent_id" in data

def test_list_agents(client, auth_headers):
    """Test listing agents."""
    response = client.get("/agents", headers=auth_headers)
    
    assert response.status_code == 200
    data = response.json()
    assert isinstance(data, list)
```

### Fixtures

```python
# tests/conftest.py
import pytest
from sqlalchemy import create_engine
from sqlalchemy.orm import sessionmaker
from api.database import Base, get_db_session

@pytest.fixture(scope="session")
def engine():
    """Create test database engine."""
    engine = create_engine("mysql://root:111@localhost:6001/mo_agent_test")
    Base.metadata.create_all(engine)
    yield engine
    Base.metadata.drop_all(engine)

@pytest.fixture
def db_session(engine):
    """Create database session for test."""
    Session = sessionmaker(bind=engine)
    session = Session()
    yield session
    session.rollback()
    session.close()

@pytest.fixture
def mock_llm():
    """Mock LLM client."""
    class MockLLM:
        def complete(self, prompt):
            return "Mocked response"
    
    return MockLLM()
```

## Test Coverage

### Generate Coverage Report

```bash
# Run tests with coverage
pytest --cov=core --cov=api

# Generate HTML report
pytest --cov=core --cov=api --cov-report=html

# Open report
open htmlcov/index.html
```

### Coverage Configuration

```ini
# .coveragerc
[run]
source = core, api
omit =
    */tests/*
    */migrations/*
    */__pycache__/*

[report]
exclude_lines =
    pragma: no cover
    def __repr__
    raise AssertionError
    raise NotImplementedError
    if __name__ == .__main__.:
```

### Coverage Goals

- **Overall**: > 80%
- **Core modules**: > 90%
- **API endpoints**: > 85%
- **Critical paths**: 100%

## Continuous Integration

### GitHub Actions

```yaml
# .github/workflows/test.yml
name: Tests

on: [push, pull_request]

jobs:
  test:
    runs-on: ubuntu-latest
    
    services:
      matrixone:
        image: matrixorigin/matrixone:latest
        ports:
          - 6001:6001
      
      redis:
        image: redis:7-alpine
        ports:
          - 6379:6379
    
    steps:
      - uses: actions/checkout@v3
      
      - name: Set up Python
        uses: actions/setup-python@v4
        with:
          python-version: '3.11'
      
      - name: Install dependencies
        run: |
          pip install -e .
          pip install pytest pytest-cov
      
      - name: Run tests
        run: pytest --cov=core --cov=api --cov-report=xml
      
      - name: Upload coverage
        uses: codecov/codecov-action@v3
```

## Best Practices

### Test Organization

1. **One test file per module**: `core/auth.py` → `tests/unit/test_auth.py`
2. **One test class per class**: `class UserManager` → `class TestUserManager`
3. **Descriptive test names**: `test_user_login_with_valid_credentials`
4. **Arrange-Act-Assert pattern**:
   ```python
   def test_something():
       # Arrange
       user = create_user()
       
       # Act
       result = user.do_something()
       
       # Assert
       assert result == expected
   ```

### Test Independence

- Each test should be independent
- Use fixtures for setup/teardown
- Don't rely on test execution order
- Clean up after tests

```python
@pytest.fixture
def temp_file():
    """Create temporary file."""
    file_path = "/tmp/test_file.txt"
    with open(file_path, "w") as f:
        f.write("test content")
    
    yield file_path
    
    # Cleanup
    if os.path.exists(file_path):
        os.remove(file_path)
```

### Mocking

Use mocks for external dependencies:

```python
from unittest.mock import Mock, patch

def test_api_call():
    """Test API call with mocked response."""
    with patch('requests.get') as mock_get:
        mock_get.return_value.json.return_value = {"status": "ok"}
        
        result = fetch_data()
        
        assert result["status"] == "ok"
        mock_get.assert_called_once()
```

### Parametrized Tests

Test multiple inputs efficiently:

```python
@pytest.mark.parametrize("input,expected", [
    ("hello", "HELLO"),
    ("world", "WORLD"),
    ("", ""),
    (None, None),
])
def test_uppercase(input, expected):
    """Test uppercase conversion."""
    result = uppercase(input)
    assert result == expected
```

## Debugging Tests

### Run with Debugger

```bash
# Run with pdb
pytest --pdb

# Drop into debugger on failure
pytest --pdb --maxfail=1

# Use ipdb (better debugger)
pip install ipdb
pytest --pdb --pdbcls=IPython.terminal.debugger:Pdb
```

### Print Debugging

```python
def test_something():
    result = do_something()
    print(f"Result: {result}")  # Will show with pytest -s
    assert result == expected
```

### Logging

```python
import logging

def test_with_logging(caplog):
    """Test with log capture."""
    with caplog.at_level(logging.INFO):
        do_something()
    
    assert "Expected log message" in caplog.text
```

## Performance Testing

### Benchmark Tests

```python
import pytest

@pytest.mark.benchmark
def test_performance(benchmark):
    """Benchmark function performance."""
    result = benchmark(expensive_function, arg1, arg2)
    assert result is not None
```

### Load Testing

```bash
# Install locust
pip install locust

# Run load test
locust -f tests/load/locustfile.py --host=http://localhost:8000
```

## See Also

- [Development Workflow](development-workflow.md) - Development guide
- [Makefile Commands](../reference/makefile-commands.md) - Test commands
- [CI/CD](../implementation/ci.md) - Continuous integration
