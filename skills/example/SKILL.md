---
name: code-review
description: "Perform a comprehensive code review with best practices"
user_invocable: true
triggers:
  - review
  - code review
  - check code
  - audit
  - inspect
allowed_tools:
  - read_file
  - git_diff
  - git_blame
  - grep
  - symbol_search
---
# Code Review Skill

When asked to review code, follow this structured approach:

## 1. Understand the Context

First, gather information about the changes:

```bash
git diff --stat HEAD~1
```

- Identify which files were changed and why
- Check the commit message for context
- Understand the scope of changes

## 2. Check for Common Issues

### Security
- [ ] No hardcoded secrets or credentials
- [ ] Input validation present
- [ ] SQL injection prevention
- [ ] XSS prevention (if applicable)

### Code Quality
- [ ] Functions are not too long (< 50 lines ideal)
- [ ] Clear variable and function names
- [ ] No obvious code duplication
- [ ] Error handling is comprehensive

### Testing
- [ ] Tests exist for new functionality
- [ ] Edge cases are covered
- [ ] Tests are meaningful (not just coverage)

## 3. Performance Considerations

- Look for N+1 queries
- Check for unnecessary loops
- Identify potential memory leaks
- Consider async/await usage

## 4. Documentation

- Public APIs should be documented
- Complex logic should have comments
- README updated if needed

## 5. Provide Feedback

Format your review as:

```
## Summary
Brief overview of the changes.

## Issues Found
### 🔴 Critical
- [File:Line] Description

### 🟡 Important  
- [File:Line] Description

### 🟢 Suggestions
- [File:Line] Description

## What's Good
Positive aspects of the code.
```

Remember: Be constructive, specific, and respectful.
