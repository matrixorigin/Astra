# review-changes

Context-aware code review combining git diffs with **astra's code intelligence** system.
Goes beyond line-by-line diff — understands symbol relationships, import changes,
trait implementations, and call graph impact.

## Usage

```
/skill review-changes
/skill review-changes --target staged --focus security
/skill review-changes --target branch:feature-x
/skill review-changes --target commit:abc123 --focus bugs
```

## Philosophy

**High signal-to-noise ratio.** Only surfaces issues that genuinely matter:
- 🔴 Bugs, security vulnerabilities, data loss, API breakage
- 🟡 Missing tests, error handling gaps, logic concerns
- 💡 Performance, alternative approaches

**Never comments on:** formatting, whitespace, style, naming (unless actively misleading).

## What It Checks

| Phase | Analysis |
|-------|----------|
| **Structural** | Symbol-level impact, public API changes, import/dependency changes, callers affected |
| **Deep review** | Logic errors, concurrency issues, error handling, off-by-one, null safety |
| **Security** | Command injection, path traversal, credential exposure, SQL injection, unsafe blocks |
| **Tests** | Coverage of changed functions, edge cases, mock updates |
| **Cross-file** | Interface contracts, error propagation, config consistency |
| **Astra-specific** | Tool registration, journal compat, state machine changes, cloud sync impact |

## Code Intelligence

Uses astra's `code_intel.rs` (9 languages) for:
- Symbol extraction (functions, types, traits, imports)
- Call site analysis (who calls the changed function?)
- Import resolution (are dependencies correct?)
- Trait implementation tracking (Rust)
