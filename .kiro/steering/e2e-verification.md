---
inclusion: always
---

# End-to-End Verification Rules

Use the real Rust API path for end-to-end confidence.

## Primary Commands

```bash
make dev-start
make test
make test-integration
make migration-contract-test
```

## Verification Expectations

- prefer the narrowest relevant contract test while iterating
- expand to `make test-integration` for API-shell behavior changes
- finish with `make test` and `make check` before handoff
- if persistence is involved, verify the stored fields that matter, not just success status

## Anti-Patterns

- relying on stale pytest-era guidance for the main server
- stopping at superficial HTTP assertions when the change affects stored state
- skipping the full Rust suite after a structural refactor
