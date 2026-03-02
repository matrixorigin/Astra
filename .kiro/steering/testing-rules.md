---
inclusion: always
---

# Testing Rules

## Test Coverage Requirements

**Mandatory Testing:**
- ✅ All new features must include tests
- ✅ Bug fixes must include regression tests
- ✅ API endpoints require integration tests
- ✅ Maintain 80%+ test coverage
- ❌ Do NOT commit code that breaks existing tests

**When to Skip:**
- Trivial getters/setters
- Auto-generated code
- Temporary debugging code

**⚠️ CRITICAL: If tests are hard to write, fix the design**

### Design Smells That Make Testing Hard

```python
# ❌ Bad: Tight coupling, hard to test
class SkillManager:
    def __init__(self):
        self.db = create_engine("postgresql://...")  # Hardcoded!
        self.api_client = HTTPClient()  # Can't mock!
        
    def install_skill(self, skill_id):
        # Directly calls external API
        data = requests.get(f"https://api.example.com/skills/{skill_id}")
        self.db.execute(...)

# ✅ Good: Dependency injection, easy to test
class SkillManager:
    def __init__(self, db_factory, api_client):
        self.db_factory = db_factory
        self.api_client = api_client
        
    def install_skill(self, skill_id):
        data = self.api_client.get_skill(skill_id)
        db = self.db_factory()
        db.execute(...)
```

### When Tests Fail: Decision Tree

```
Test is failing
    ├─ Is the test wrong?
    │   └─ Fix the test (rare)
    │
    ├─ Is the code wrong?
    │   └─ Fix the code (common)
    │
    └─ Is the design wrong?
        ├─ Too tightly coupled? → Add dependency injection
        ├─ Too many responsibilities? → Split into smaller classes
        ├─ Hidden dependencies? → Make them explicit
        ├─ Global state? → Pass state as parameters
        └─ Hard to mock? → Extract interface/protocol

❌ NEVER: Lower test requirements
❌ NEVER: Skip tests to make CI pass
❌ NEVER: Reduce coverage to avoid failures
✅ ALWAYS: Refactor design to make testing easier
```

### Examples: Fix Design, Not Tests

**Problem: Can't test because of hardcoded database**
```python
# ❌ Bad: Skip test because "it's too hard"
@pytest.mark.skip("Can't test with real database")
def test_create_user():
    pass

# ✅ Good: Refactor to accept database connection
class UserManager:
    def __init__(self, db_factory):  # Inject dependency
        self.db_factory = db_factory
        
def test_create_user(db_factory):  # Now testable!
    mgr = UserManager(db_factory)
    user = mgr.create_user("alice")
    assert user.name == "alice"
```

**Problem: Can't test because of external API calls**
```python
# ❌ Bad: Lower coverage because "external dependency"
def fetch_and_process(skill_id):
    data = requests.get(f"https://api.example.com/skills/{skill_id}")
    # No test coverage here

# ✅ Good: Extract interface, inject client
class SkillAPI(Protocol):
    def get_skill(self, skill_id: str) -> dict: ...

def fetch_and_process(skill_id: str, api_client: SkillAPI):
    data = api_client.get_skill(skill_id)
    # Now fully testable with mock client
```

**Problem: Can't test because of global state**
```python
# ❌ Bad: Tests interfere with each other
_cache = {}  # Global state!

def get_skill(skill_id):
    if skill_id in _cache:
        return _cache[skill_id]
    # Tests pollute each other's cache

# ✅ Good: Pass state explicitly or use class
class SkillCache:
    def __init__(self):
        self._cache = {}
        
    def get_skill(self, skill_id):
        if skill_id in self._cache:
            return self._cache[skill_id]
        # Each test gets fresh instance
```

### Red Flags: Design Needs Refactoring

If you find yourself thinking:
- "This is too hard to test" → Design is too coupled
- "I need to mock 10 things" → Too many dependencies
- "Tests are flaky" → Hidden state or race conditions
- "Can't test without real database/API" → Missing abstraction
- "Need to skip this test" → Design smell

**Solution: Refactor first, then test**
1. Identify the coupling/dependency issue
2. Extract interface or add dependency injection
3. Write the test (now it's easy)
4. Implement the feature

## Test Structure

### File Organization
```
tests/
├── unit/              # Fast, isolated tests
│   ├── test_*.py      # One file per module
│   └── conftest.py    # Shared fixtures
├── integration/       # E2E tests with real DB
│   ├── test_*_e2e.py
│   └── api/           # API endpoint tests
└── conftest.py        # Global fixtures
```

### Naming Conventions
```python
# File: tests/unit/test_skill_manager.py
def test_install_skill_success():           # Happy path
def test_install_skill_missing_permission(): # Error case
def test_install_skill_already_installed():  # Edge case

# Use descriptive names: test_<action>_<condition>_<expected>
```

### Test Pattern (AAA)
```python
def test_create_session():
    # Arrange - Setup test data
    user_id = "alice"
    agent_id = "dev-agent"
    
    # Act - Execute the operation
    session = session_mgr.create_session(user_id, agent_id)
    
    # Assert - Verify results
    assert session.user_id == user_id
    assert session.status == "active"
```

## Fixtures & Setup

### Use Real Database (Not Mocks)
```python
@pytest.fixture
def db_factory(test_engine):
    """Use real MatrixOne database for tests."""
    SessionLocal = sessionmaker(bind=test_engine)
    return SessionLocal

# ✅ Good: Test with real DB
def test_event_persistence(db_factory):
    db = db_factory()
    event = create_event(db, ...)
    assert db.query(Event).filter_by(id=event.id).first()

# ❌ Avoid: Excessive mocking hides integration issues
def test_event_persistence_mocked():
    mock_db = MagicMock()
    # ... too much mocking
```

### Fixture Scope
```python
@pytest.fixture(scope="session")  # Once per test session
def test_engine(): ...

@pytest.fixture(scope="function")  # Default: once per test
def db(): ...

@pytest.fixture(autouse=True)  # Auto-run for cleanup
def cleanup(): ...
```

### Test Isolation
```python
@pytest.fixture
def clean_db(db):
    """Clean up before and after each test."""
    # Cleanup before
    db.execute(delete(Event))
    db.commit()
    
    yield db
    
    # Cleanup after
    db.execute(delete(Event))
    db.commit()
```

## Async Tests

```python
@pytest.mark.asyncio
async def test_stream_response():
    async with client.stream("POST", "/chat") as response:
        chunks = []
        async for chunk in response.aiter_text():
            chunks.append(chunk)
    assert len(chunks) > 0
```

## Database Tests

### Use Transactions for Speed
```python
def test_bulk_insert(db):
    # Use bulk operations
    events = [Event(...) for _ in range(100)]
    db.bulk_save_objects(events)
    db.commit()
    
    assert db.query(Event).count() == 100
```

### Test Data Versioning
```python
def test_time_travel_query(db):
    # Create snapshot
    snapshot_id = create_snapshot(db)
    
    # Modify data
    update_data(db)
    
    # Query historical state
    old_data = query_at_snapshot(db, snapshot_id)
    assert old_data == original_state
```

## API Tests

```python
def test_chat_endpoint(client, auth_headers):
    response = client.post(
        "/chat",
        json={"message": "Hello", "session_id": "test-123"},
        headers=auth_headers
    )
    assert response.status_code == 200
    assert "run_id" in response.json()
```

## Performance Guidelines

- Unit tests should run in < 100ms each
- Integration tests < 5s each
- Mark slow tests: `@pytest.mark.slow`
- Use parallel testing: `pytest -n auto`

## Common Patterns

### Testing Errors
```python
def test_permission_denied():
    with pytest.raises(PermissionDeniedError) as exc_info:
        skill_mgr.install_skill(user_id="guest", skill_id="admin-only")
    assert "permission denied" in str(exc_info.value).lower()
```

### Parametrized Tests
```python
@pytest.mark.parametrize("input,expected", [
    ("valid", True),
    ("invalid", False),
    ("", False),
])
def test_validation(input, expected):
    assert validate(input) == expected
```

### Testing Streaming
```python
def test_sse_stream():
    events = []
    for event in stream_events():
        events.append(event)
        if event.type == "done":
            break
    assert len(events) > 0
    assert events[-1].type == "done"
```

## Running Tests

### Development Workflow (Incremental Testing)

**⚠️ CRITICAL: Do NOT run full test suite during debugging!**

```bash
# 1. Debug Phase - Run ONLY the failing test
pytest tests/unit/test_skill_manager.py::test_install_skill_success -v

# 2. Fix and verify - Run related tests only
pytest tests/unit/test_skill_manager.py -n auto

# 3. Expand scope - Run module tests
pytest tests/unit/ -k "skill" -n auto

# 4. Final verification - Run full suite (ONLY when confident)
make dev-test-keep
```

### Always Use Parallel Execution

**✅ REQUIRED: Always use `-n auto` for faster feedback**

```bash
# ✅ Good: Parallel execution (4-8x faster)
pytest -n auto

# ❌ Bad: Sequential execution (slow, wastes time)
pytest

# Exception: Only run sequential when debugging race conditions
pytest --dist=no
```

### Test Commands

```bash
# Specific test file (parallel)
pytest tests/unit/test_skill_manager.py -n auto

# Specific test function (single test, no parallel needed)
pytest tests/unit/test_skill_manager.py::test_install_skill_success -v

# Pattern matching (parallel)
pytest -k "skill_manager" -n auto

# With coverage report (parallel)
pytest --cov=core --cov-report=html -n auto

# Stop on first failure (useful for debugging)
pytest -x -n auto

# Show print statements
pytest -s -n auto

# Run only failed tests from last run
pytest --lf -n auto
```

## Pre-Commit Checklist

**⚠️ Follow this order - do NOT skip to full test suite!**

### 1. Local Development (Incremental)
- [ ] Run specific failing test: `pytest path/to/test.py::test_name -v`
- [ ] Fix and verify: `pytest path/to/test.py -n auto`
- [ ] Run related tests: `pytest -k "module_name" -n auto`
- [ ] Check warnings: `pytest -W error -n auto path/to/test.py`

### 2. Pre-Commit (Expanded)
- [ ] Run affected module tests: `pytest tests/unit/test_module.py -n auto`
- [ ] Linting: `make lint` (must pass)
- [ ] Type checking: `make type-check` (must pass)
- [ ] No warnings: `pytest -W all -n auto tests/unit/`

### 3. Final Verification (Full Suite)
- [ ] **ONLY NOW**: Run full test suite: `make dev-test-keep`
- [ ] Coverage check: `pytest --cov=core --cov-report=term -n auto`
- [ ] Integration tests: `make dev-test-integration`

### 4. Commit
- [ ] All tests pass
- [ ] Coverage maintained (80%+)
- [ ] No warnings
- [ ] New tests added for new features
- [ ] Regression tests added for bug fixes
- [ ] **If tests were hard to write, design was refactored first**

**Time Budget:**
- Debug phase: 1-5 minutes per iteration (specific tests only)
- Pre-commit: 5-10 minutes (module tests)
- Final verification: 10-15 minutes (full suite)

**❌ NEVER:**
- Skip directly to full test suite when debugging
- Commit with failing tests
- Commit with warnings
- Skip tests with `@pytest.mark.skip` to make CI pass
- Reduce test coverage to fix failures
- Lower requirements because "it's hard to test" - refactor design instead

## Warnings Are Errors

**⚠️ CRITICAL: Never ignore warnings - they become bugs**

```bash
# Treat warnings as errors
pytest -W error -n auto

# Show all warnings
pytest -W all -n auto

# Common warnings to fix immediately:
# - DeprecationWarning: Update deprecated API usage
# - ResourceWarning: Unclosed files/connections
# - PytestUnraisableExceptionWarning: Async cleanup issues
# - SQLAlchemy warnings: Query inefficiencies
```

### Fix Warnings Immediately

```python
# ❌ Bad: Ignoring ResourceWarning
def test_read_file():
    f = open("test.txt")
    data = f.read()
    # Missing f.close() - ResourceWarning!

# ✅ Good: Proper cleanup
def test_read_file():
    with open("test.txt") as f:
        data = f.read()
    # Auto-closed, no warning

# ❌ Bad: Ignoring DeprecationWarning
result = old_deprecated_function()

# ✅ Good: Use new API
result = new_recommended_function()
```

## What NOT to Test

- Third-party library internals
- Database engine behavior
- Python standard library
- Auto-generated Pydantic models (unless custom logic)
- Simple data classes without logic

**⚠️ WARNING: Do NOT skip tests to "simplify" or "save time"**
- ❌ Never skip failing tests with `@pytest.mark.skip`
- ❌ Never comment out assertions
- ❌ Never reduce test coverage to make tests pass
- ❌ Never lower requirements because "it's hard to test"
- ✅ Fix the root cause instead
- ✅ Refactor design to make testing easier
- ✅ Add dependency injection if needed
- ✅ Extract interfaces for external dependencies

**If a test is hard to write, the design is wrong - fix the design, not the test.**

## Test Data

### Use Factories for Complex Objects
```python
def create_test_user(db, **overrides):
    defaults = {
        "user_id": f"test-{uuid.uuid4()}",
        "email": "test@example.com",
        "created_at": datetime.now(timezone.utc)
    }
    user = User(**{**defaults, **overrides})
    db.add(user)
    db.commit()
    return user
```

### Avoid Hardcoded IDs
```python
# ❌ Bad: Hardcoded IDs cause conflicts
def test_create_user():
    user = User(id=1, name="Alice")

# ✅ Good: Generate unique IDs
def test_create_user():
    user = User(id=str(uuid.uuid4()), name="Alice")
```

## Debugging Failed Tests

**⚠️ Start small, expand gradually - NEVER debug with full test suite**

### Step 1: Isolate the Failure
```bash
# Run ONLY the failing test with verbose output
pytest tests/unit/test_skill_manager.py::test_install_skill_success -vv

# Show local variables on failure
pytest tests/unit/test_skill_manager.py::test_install_skill_success -vv -l

# Drop into debugger on failure
pytest tests/unit/test_skill_manager.py::test_install_skill_success --pdb
```

### Step 2: Understand the Context
```bash
# Show print statements and logging
pytest tests/unit/test_skill_manager.py::test_install_skill_success -s -vv

# Show full diff on assertion failures
pytest tests/unit/test_skill_manager.py::test_install_skill_success -vv --tb=long
```

### Step 3: Verify Fix (Incremental)
```bash
# 1. Run the fixed test
pytest tests/unit/test_skill_manager.py::test_install_skill_success -n auto

# 2. Run related tests in same file
pytest tests/unit/test_skill_manager.py -n auto

# 3. Run all tests matching pattern
pytest -k "skill_manager" -n auto

# 4. ONLY THEN run full suite
make dev-test-keep
```

### Common Debug Patterns

```bash
# Run only failed tests from last run (fast iteration)
pytest --lf -n auto

# Run failed tests first, then all others
pytest --ff -n auto

# Stop on first failure (find root cause faster)
pytest -x -n auto

# Show 10 slowest tests (identify bottlenecks)
pytest --durations=10 -n auto

# Disable parallel for race condition debugging
pytest tests/unit/test_skill_manager.py --dist=no -vv
```

### Warning Debugging

```bash
# Show all warnings with full traceback
pytest -W all -n auto --tb=long

# Treat warnings as errors (fail fast)
pytest -W error -n auto

# Show specific warning category
pytest -W default::DeprecationWarning -n auto
```

**Time-Saving Tips:**
- ✅ Single test iteration: 1-5 seconds
- ✅ File-level tests: 10-30 seconds  
- ✅ Module tests: 1-2 minutes
- ❌ Full suite: 10-15 minutes (ONLY for final verification)
