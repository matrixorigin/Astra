---
inclusion: always
---

# Project-Specific Coding Standards

## Error Handling

**Always use moerr package for errors:**
```python
from core.errors import moerr

# Correct
raise moerr.NewInternalError(ctx, "operation failed")

# Wrong - don't use standard exceptions directly
raise Exception("operation failed")
```

## Database Operations

- Always use context managers for database sessions
- Use transactions for multi-step operations
- Close sessions explicitly in finally blocks

## API Development

- All endpoints must have authentication
- Use Pydantic models for request/response validation
- Return appropriate HTTP status codes
- Log all errors with context

## Documentation

- Add docstrings to all public functions/classes
- Include type hints for all function parameters
- Update API documentation when adding endpoints
