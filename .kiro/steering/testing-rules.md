---
inclusion: always
---

# Testing Rules

## Mandatory Expectations

- Every non-trivial change must have appropriate test coverage.
- Bug fixes need regression coverage when feasible.
- API behavior changes should be validated in `rust/crates/api-shell/tests/`.
- Do not weaken or skip tests just to make a change pass.

## Preferred Test Ladder

```text
single contract test -> affected contract group -> full API-shell suite -> full workspace
```

## Commands

```bash
cargo test --manifest-path rust/Cargo.toml -p mo-agent-runtime --test auth_contract
make test-integration
make test
make check
```

## What Good Tests Do

- assert behavior, not just execution
- verify meaningful persisted fields when DB state is involved
- cover success and failure paths
- use descriptive names tied to current runtime behavior

## What To Reject

- stale compatibility naming that no longer matches the implementation
- tests that only assert `is_ok()` or `status == 200` when deeper state should be checked
- broad fixture magic that hides important setup
- skipping validation for risky refactors

## Design Signal

If tests are painful to write, the design may still be too coupled. Prefer extracting seams over lowering coverage.
