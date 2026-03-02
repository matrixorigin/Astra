---
inclusion: always
---

# Code Review Guidelines

**Philosophy: Uncompromising Quality - No Shortcuts, No Excuses**

## Review Priority (Critical → Important → Nice-to-have)

### 🔴 CRITICAL: Block merge immediately

1. **Design Flaws** - Architecture problems that will haunt us later
2. **Test Quality** - Fake tests, low-quality tests, missing coverage
3. **Security Issues** - SQL injection, permission bypass, data leaks
4. **Data Integrity** - Event sourcing violations, audit trail gaps
5. **Resource Leaks** - Unclosed connections, memory leaks

### 🟡 IMPORTANT: Must fix before merge

6. **Functionality Bugs** - Logic errors, edge cases
7. **Performance Issues** - N+1 queries, inefficient algorithms
8. **Error Handling** - Silent failures, wrong error types
9. **Testability** - Tight coupling, hidden dependencies

### 🟢 NICE-TO-HAVE: Can fix later

10. **Code Style** - Naming, formatting (auto-fixable)
11. **Documentation** - Comments, docstrings
12. **Refactoring** - DRY violations, minor optimizations

---

## 🔴 CRITICAL REVIEW AREAS

### 1. Design Quality - No Compromises

**❌ REJECT: Design smells that make code untestable**

```python
# ❌ REJECT: Tight coupling, hardcoded dependencies
class SkillManager:
    def __init__(self):
        self.db = create_engine("postgresql://...")  # Hardcoded!
        self.cache = {}  # Global state!
        
    def install_skill(self, skill_id):
        # Direct HTTP call, can't test
        data = requests.get(f"https://api.example.com/{skill_id}")

# ✅ APPROVE: Dependency injection, testable
class SkillManager:
    def __init__(self, db_factory: Callable, api_client: SkillAPI):
        self.db_factory = db_factory
        self.api_client = api_client
        
    def install_skill(self, skill_id: str) -> Skill:
        data = self.api_client.get_skill(skill_id)
        # Fully testable with mock client
```

**Red Flags - Request Design Changes:**
- "This is hard to test" → Design is wrong, refactor first
- Too many parameters (>5) → Missing abstraction
- God class (>500 lines) → Split responsibilities
- Circular dependencies → Rethink module boundaries
- Global state → Pass state explicitly
- Hidden side effects → Make effects explicit

**Review Questions:**
- Can this be tested without mocking 10 things?
- Is the responsibility clear and single?
- Would I understand this in 6 months?
- Is there a simpler design?

---

### 2. Test Quality - Zero Tolerance for Fake Tests

**❌ REJECT: Tests that don't actually test**

```python
# ❌ REJECT: Fake test - just calls function, no assertions
def test_install_skill():
    mgr.install_skill("skill-123")
    # No assertions! This tests nothing!

# ❌ REJECT: Meaningless assertion
def test_install_skill():
    result = mgr.install_skill("skill-123")
    assert result is not None  # Too weak!

# ❌ REJECT: Over-mocked test - tests mocks, not code
def test_install_skill():
    mock_db = MagicMock()
    mock_api = MagicMock()
    mock_cache = MagicMock()
    mock_logger = MagicMock()
    # ... 10 more mocks
    # This tests mock interactions, not real behavior!

# ❌ REJECT: Test that always passes
def test_install_skill():
    try:
        mgr.install_skill("skill-123")
        assert True  # Meaningless!
    except:
        pass  # Swallows all errors!

# ✅ APPROVE: Real test with meaningful assertions
def test_install_skill_creates_database_record(db_factory):
    mgr = SkillManager(db_factory, mock_api_client)
    
    skill = mgr.install_skill("skill-123")
    
    # Verify actual database state
    db = db_factory()
    saved = db.query(SkillInstallation).filter_by(id=skill.id).first()
    assert saved is not None
    assert saved.skill_id == "skill-123"
    assert saved.status == "installed"
    
# ✅ APPROVE: Tests error cases
def test_install_skill_fails_without_permission(db_factory):
    mgr = SkillManager(db_factory, mock_api_client)
    
    with pytest.raises(PermissionDeniedError) as exc:
        mgr.install_skill("admin-only-skill", user_id="guest")
    
    assert "permission denied" in str(exc.value).lower()
    # Verify no partial state left behind
    db = db_factory()
    assert db.query(SkillInstallation).count() == 0
```

**Test Quality Checklist - ALL must be YES:**
- [ ] Tests actual behavior, not implementation details?
- [ ] Has meaningful assertions (not just `assert True`)?
- [ ] Tests both success and failure cases?
- [ ] Uses real database/dependencies where possible?
- [ ] Verifies side effects (DB writes, events logged)?
- [ ] Tests edge cases (empty input, null, boundary values)?
- [ ] Test name describes what is being tested?
- [ ] Would catch real bugs if code breaks?

**Red Flags - Request Better Tests:**
- No assertions or only `assert True`
- Try/except that swallows all errors
- More than 50% of code is mocks
- Tests pass even when implementation is deleted
- No error case testing
- No edge case testing
- Test name is generic (`test_function_works`)

**Review Questions:**
- If I break the implementation, will this test fail?
- Does this test verify actual behavior or just mock calls?
- Are error cases covered?
- Is this testing the right thing?

---

### 3. Security - No Exceptions

**❌ REJECT: Security vulnerabilities**

```python
# ❌ REJECT: SQL injection
def get_user(user_id):
    query = f"SELECT * FROM users WHERE id = '{user_id}'"
    return db.execute(query)

# ✅ APPROVE: Parameterized query
def get_user(user_id):
    return db.query(User).filter_by(id=user_id).first()

# ❌ REJECT: Missing permission check
def delete_skill(skill_id):
    db.delete(Skill, id=skill_id)

# ✅ APPROVE: Permission check
def delete_skill(skill_id, user_id):
    if not has_permission(user_id, "skill:delete"):
        raise PermissionDeniedError()
    db.delete(Skill, id=skill_id)

# ❌ REJECT: Logging sensitive data
logger.info(f"User login: {username}, password: {password}")

# ✅ APPROVE: No sensitive data in logs
logger.info(f"User login: {username}")
```

**Security Checklist:**
- [ ] No SQL injection (use ORM or parameterized queries)
- [ ] Permission checks before sensitive operations
- [ ] No sensitive data in logs (passwords, tokens, PII)
- [ ] Input validation for all user input
- [ ] No hardcoded secrets (use environment variables)
- [ ] Rate limiting for expensive operations
- [ ] Audit trail for security-relevant actions

---

### 4. Data Integrity - Event Sourcing & Audit

**❌ REJECT: Bypassing event system**

```python
# ❌ REJECT: Direct DB update, no audit trail
def update_skill_status(skill_id, status):
    db.execute(f"UPDATE skills SET status = '{status}' WHERE id = '{skill_id}'")
    # No event logged! Audit trail broken!

# ✅ APPROVE: Event-driven update
def update_skill_status(skill_id, status, user_id):
    # 1. Log event
    event = logger.create_skill_status_change_event(
        skill_id=skill_id,
        old_status=current_status,
        new_status=status,
        user_id=user_id,
        causal_chain_id=chain_id
    )
    
    # 2. Update state
    db.execute(update(Skill).where(Skill.id == skill_id).values(status=status))
    
    # 3. Return event for audit
    return event
```

**Data Integrity Checklist:**
- [ ] All state changes logged as events
- [ ] Causal chain ID propagated correctly
- [ ] Audit trail complete (who, what, when, why)
- [ ] No direct DB updates bypassing event system
- [ ] Transactions used for multi-step operations
- [ ] Rollback handling for failures

---

### 5. Resource Management - No Leaks

**❌ REJECT: Resource leaks**

```python
# ❌ REJECT: Unclosed database connection
def get_users():
    db = SessionLocal()
    users = db.query(User).all()
    return users  # db never closed!

# ✅ APPROVE: Proper cleanup
def get_users():
    db = SessionLocal()
    try:
        users = db.query(User).all()
        return users
    finally:
        db.close()

# ✅ BETTER: Context manager
def get_users():
    with SessionLocal() as db:
        return db.query(User).all()

# ❌ REJECT: File not closed
def read_config():
    f = open("config.json")
    data = json.load(f)
    return data  # f never closed!

# ✅ APPROVE: Context manager
def read_config():
    with open("config.json") as f:
        return json.load(f)
```

**Resource Checklist:**
- [ ] Database connections closed (use context managers)
- [ ] Files closed (use `with` statement)
- [ ] HTTP connections closed
- [ ] Async tasks properly awaited
- [ ] No infinite loops without exit condition
- [ ] Memory-intensive operations cleaned up

---

## 🟡 IMPORTANT REVIEW AREAS

### 6. Functionality & Edge Cases

**Review Questions:**
- Does it handle empty input?
- Does it handle null/None?
- Does it handle boundary values (0, -1, max int)?
- Does it handle concurrent access?
- Does it handle partial failures?
- What happens if external service is down?

### 7. Performance

**Red Flags:**
- N+1 query problem
- Loading entire table into memory
- No pagination for large datasets
- Inefficient algorithms (O(n²) when O(n) possible)
- No caching for expensive operations
- Synchronous calls in async context

### 8. Error Handling

```python
# ❌ REJECT: Silent failure
def process_data(data):
    try:
        result = expensive_operation(data)
    except:
        pass  # Error swallowed!

# ❌ REJECT: Wrong error type
def get_user(user_id):
    user = db.query(User).filter_by(id=user_id).first()
    if not user:
        raise Exception("Not found")  # Use specific error!

# ✅ APPROVE: Proper error handling
def get_user(user_id):
    user = db.query(User).filter_by(id=user_id).first()
    if not user:
        raise UserNotFoundError(f"User {user_id} not found")
```

---

## Before Submitting Code (Author Self-Review)

**⚠️ Complete ALL items before requesting review:**

### Design & Architecture
- [ ] Design is simple and testable (no tight coupling)
- [ ] Responsibilities are clear and single
- [ ] Dependencies are injected, not hardcoded
- [ ] No global state or hidden side effects
- [ ] Follows existing patterns in codebase

### Test Quality (CRITICAL)
- [ ] Tests have meaningful assertions (not just `assert True`)
- [ ] Tests verify actual behavior, not mocks
- [ ] Both success and error cases tested
- [ ] Edge cases covered (null, empty, boundary)
- [ ] Tests use real database where possible
- [ ] Test names are descriptive
- [ ] All tests pass: `pytest -n auto -W error`
- [ ] Coverage maintained: `pytest --cov`

### Code Quality
- [ ] No debug print statements or commented code
- [ ] Linting passes: `make lint`
- [ ] Type checking passes: `make type-check`
- [ ] Variable/function names are descriptive
- [ ] Complex logic has comments explaining "why"
- [ ] No code duplication (DRY)

### Security & Data Integrity
- [ ] No SQL injection (use ORM)
- [ ] Permission checks before sensitive operations
- [ ] All state changes logged as events
- [ ] Audit trail complete
- [ ] No sensitive data in logs

### Resource Management
- [ ] Database connections closed (context managers)
- [ ] Files closed (`with` statement)
- [ ] No resource leaks

### Documentation
- [ ] Public functions have docstrings
- [ ] API changes documented
- [ ] Breaking changes highlighted

---

## Reviewer Checklist

### First Pass: Critical Issues (Block merge)
1. **Design**: Is the design sound and testable?
2. **Tests**: Are tests real and meaningful?
3. **Security**: Any vulnerabilities?
4. **Data Integrity**: Event sourcing followed?
5. **Resource Leaks**: Proper cleanup?

### Second Pass: Important Issues (Must fix)
6. **Functionality**: Does it work correctly?
7. **Edge Cases**: All cases handled?
8. **Performance**: Any obvious bottlenecks?
9. **Error Handling**: Proper error types and handling?

### Third Pass: Nice-to-have (Can defer)
10. **Code Style**: Naming, formatting
11. **Documentation**: Comments, docstrings
12. **Refactoring**: Minor improvements

---

## How to Give Feedback

### Be Specific and Constructive

```
❌ Bad: "This code is bad"
✅ Good: "This function has tight coupling to the database. 
         Consider injecting db_factory as a parameter to make it testable."

❌ Bad: "Tests are weak"
✅ Good: "This test only asserts `result is not None`, which is too weak.
         Please verify the actual skill_id and status in the database."

❌ Bad: "Fix the design"
✅ Good: "This class has 3 responsibilities (API calls, DB access, caching).
         Consider splitting into SkillAPI, SkillRepository, and SkillCache."
```

### Severity Levels

- **CRITICAL (🔴)**: Must fix before merge, blocks deployment
- **IMPORTANT (🟡)**: Must fix before merge, but not blocking
- **SUGGESTION (🟢)**: Nice to have, can be separate PR
- **QUESTION (❓)**: Asking for clarification

---

## Handling Disagreements

### When Author Disagrees with Review

1. **Understand the concern**: Ask reviewer to clarify
2. **Provide context**: Explain your reasoning
3. **Seek third opinion**: If still disagree, ask team lead
4. **Default to quality**: When in doubt, choose higher quality

### When Reviewer is Wrong

- Politely explain with evidence
- Show test results or benchmarks
- Reference existing patterns in codebase
- Escalate to team lead if needed

### Non-Negotiable

These are **NEVER** acceptable, no matter the excuse:
- ❌ Fake tests (no assertions)
- ❌ Skipping tests to make CI pass
- ❌ Security vulnerabilities
- ❌ Breaking audit trail
- ❌ Resource leaks
- ❌ Untestable design (tight coupling)

**"We're in a hurry" is not a reason to lower quality.**
**"It's hard to test" means the design is wrong - refactor first.**

---

## Review Response Time

- **Critical PRs** (hotfixes): < 2 hours
- **Normal PRs**: < 1 business day
- **Large PRs** (>500 lines): < 2 business days

**If PR is too large (>500 lines), ask author to split it.**

---

## Post-Merge Reflection

**After every PR merge, take 5 minutes to reflect:**

### What Went Well?
- What design decisions worked out?
- What made the code easy to test?
- What patterns should we reuse?

### What Could Be Better?
- What took longer than expected? Why?
- What design issues did reviewers catch?
- What tests were initially weak?
- What would I do differently next time?

### Lessons Learned
- Did I rush and compromise quality?
- Did I refactor when tests were hard to write?
- Did I write real tests or just coverage tests?
- Did I follow the incremental testing workflow?

### Action Items
- [ ] Update documentation if patterns changed
- [ ] Share learnings with team
- [ ] Add to coding standards if pattern is reusable
- [ ] Schedule refactoring if technical debt was added

**Keep a personal log of reflections - review monthly to identify patterns.**

---

## Continuous Improvement

### Monthly Team Review
- Review rejected PRs: What patterns led to rejection?
- Review slow PRs: What caused delays?
- Review bugs in production: What did reviews miss?
- Update guidelines based on learnings

### Metrics to Track
- Average PR review time
- Number of review iterations per PR
- Test quality issues caught in review
- Design issues caught in review
- Bugs found in production vs review

**Goal: Catch issues earlier, reduce review iterations, improve first-time quality.**
