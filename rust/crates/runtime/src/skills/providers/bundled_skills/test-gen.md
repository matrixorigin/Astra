---
name: test-gen
description: "Generate comprehensive tests for existing code — unit, integration, and edge cases"
version: "1.0.0"
context: fork
triggers:
  - "write tests"
  - "add tests"
  - "test coverage"
  - "generate tests"
  - "missing tests"
when_to_use: "When the user wants to add test coverage to existing code that lacks adequate tests"
category: testing
arguments:
  - name: TARGET
    description: "File, function, module, or feature to generate tests for"
    required: true
tags:
  - testing
  - quality
  - automation
---
# Test Generation

Generate comprehensive tests for the specified code.

## Target

$ARGUMENTS

## Process

### 1. Analyze the Target

Read and understand the code to test:
- What are the public APIs / entry points?
- What are the inputs, outputs, and side effects?
- What invariants should hold?
- What error conditions exist?
- What dependencies need mocking?

### 2. Identify Test Categories

For each function/method, determine which tests are needed:

**Happy path**: Normal inputs producing expected outputs
**Edge cases**: Empty inputs, boundary values, maximum sizes, zero, negative numbers
**Error cases**: Invalid inputs, missing dependencies, network failures, permission denied
**State transitions**: Before/after states for stateful operations
**Concurrency**: Race conditions, deadlocks (if applicable)
**Integration**: Cross-module interactions, database operations, file I/O

### 3. Check Existing Coverage

- Find existing test files for the module
- Identify what's already tested vs what's missing
- Follow the project's existing test patterns and conventions
- Use the same test framework, assertion style, and naming conventions

### 4. Generate Tests

Write tests following these principles:
- **Arrange-Act-Assert**: Clear setup, execution, and verification
- **One assertion per concept**: Each test verifies one specific behavior
- **Descriptive names**: `test_parse_empty_input_returns_error` not `test_parse_3`
- **Independent**: Tests don't depend on each other or execution order
- **Fast**: Mock expensive operations (network, disk, sleep)
- **Deterministic**: No randomness, time-dependency, or flaky conditions

### 5. Verify

Run the new tests to confirm they pass:
```
cargo test <module> -- --nocapture
```

If any test fails, fix it — a failing test is a bug in the test or the code. Determine which and fix appropriately.

### 6. Report

Summarize:
- Number of tests added
- Coverage areas: which functions/paths are now tested
- Notable findings: bugs discovered during test writing
- Remaining gaps: what still lacks coverage and why

## Rules
- Follow existing test conventions in the project — don't introduce a new framework
- Test behavior, not implementation — tests should survive refactoring
- Don't test private internals unless they're complex enough to warrant it
- Mock at boundaries (network, filesystem, database), not internal functions
- If you find a bug while writing tests, fix the bug AND write a regression test
- Don't generate trivial tests (testing that a constant equals itself)
