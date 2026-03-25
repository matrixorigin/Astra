---
inclusion: always
---

# Code Review Guidelines

## Review Priorities

1. correctness and security
2. data integrity and persistence behavior
3. test quality and regression coverage
4. architectural fit with existing Rust module boundaries
5. clarity of names, ownership, and error handling

## Look For

- logic errors and edge cases
- stale migration-era naming or comments
- hidden side effects
- broad fallbacks that mask real failures
- overgrown modules that now have a clear extraction seam

## Ask During Review

- does the test fail if the implementation is broken?
- does the naming describe the current behavior?
- does the module own one coherent responsibility?
- are all meaningful side effects validated?

## Required Validation

```bash
make check
make test
```
