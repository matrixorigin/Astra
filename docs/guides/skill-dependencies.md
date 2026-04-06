# Skill & Tool Dependency Versioning

## Overview

Skills and tools support semantic versioning with typed dependencies. Every dependency specifies a name, version constraint, and type (skill or tool). The system validates the entire dependency tree at install time — version conflicts, missing dependencies, and circular dependencies are caught before anything runs.

## Version Constraint Syntax

Follows pip-style conventions:

| Constraint | Meaning |
|-----------|---------|
| `>=1.0` | 1.0 or higher |
| `<2.0` | Below 2.0 |
| `>=1.0,<2.0` | 1.x range |
| `~=1.2.3` | >=1.2.3, <1.3.0 (compatible release) |
| `~=1.2` | >=1.2.0, <2.0.0 |
| `==1.0.0` | Exact match |
| `!=1.5.0` | Exclude specific version |
| `*` | Any version (default) |

## Dependency Declaration

### manifest.yaml (New Format)

```yaml
name: my_skill
version: "1.0.0"
description: "My skill"
depends_on:
  - name: git_status
    version: ">=1.0"
    type: tool
  - name: knowledge
    version: "~=2.1.0"
    type: skill
```

### manifest.yaml (Old Format — Still Supported)

```yaml
depends_on:
  - github
  - jira
```

Old format is auto-converted to `Dependency(name=..., version="*", type=skill)`.

## Tool Versioning

Tools have version information tracked in metadata:

```rust
// rust/crates/runtime/src/tool_registry/tool_catalog.rs
pub struct ToolMetadata {
    pub name: &'static str,
    pub version: &'static str,  // default "1.0.0"
    // ...
}
```

## What Gets Validated at Install Time

1. **Missing dependencies** — all declared deps must exist in the registry
2. **Version compatibility** — available version must satisfy the constraint
3. **Circular dependencies** — A→B→A is rejected with a clear cycle path
4. **Transitive dependencies** — the entire tree is checked, not just direct deps

## Lifecycle Validation Rules

Version constraints are not just an install-time concern. Every operation that changes a user's installed skill set must preserve the **dependency invariant**: all constraints declared by all installed skills remain satisfied.

### Upgrade

When upgrading skill X from v1 to v2:
1. **Reverse check**: find all installed skills that declare a dependency on X. Verify v2 satisfies each of their version constraints. If skill A requires `X >=1.0,<2.0` and you upgrade X to 2.0.0 → rejected.
2. **Forward check**: if v2 has different `depends_on` than v1, resolve the new dependency tree (same validation as install: cycles, missing, version conflicts).

### Rollback

Same validation as upgrade, but targeting `previous_version`. Rolling back X from v2 to v1 must not break any dependent's constraints, and v1's own dependencies must still be satisfiable.

### Uninstall

Before uninstalling skill X, check if any other installed skill depends on X. If so, reject with a clear error listing the dependents. The user must uninstall dependents first (or use `--force` to skip the check, accepting the risk).

### Runtime (Defense-in-Depth)

`require_executable()` verifies both dependency existence AND version compatibility at execution time. This is a backstop — it should never trigger if the mutation gates work correctly, but it catches edge cases like `--force` operations or direct DB edits.

### Implementation Status

| Operation | Validation | Status |
|-----------|-----------|--------|
| `install()` | Full tree: cycles, missing, versions, transitive | ✅ Implemented |
| `upgrade()` | Reverse + forward constraint check | 🔲 Not yet |
| `rollback()` | Reverse + forward constraint check | 🔲 Not yet |
| `uninstall()` | Reverse dependency check | 🔲 Not yet |
| `require_executable()` | Existence + version check | ⚠️ Existence only |

## CLI Commands

```bash
# Check what breaks when upgrading
astra skill upgrade-check <skill_name> <new_version>
```

Example:
```
$ astra skill upgrade-check knowledge 3.0.0
⚠️  Upgrading knowledge to 3.0.0 would break:
  • github (requires ~=2.1.0)
```

## Error Messages

### Missing Dependency
```
Missing dependencies for 'my_skill': git_tools, knowledge
```

### Version Conflict
```
Version conflicts:
  git_status (available 1.0.0): skill_a requires >=2.0, skill_b requires >=1.5
```

### Circular Dependency
```
Circular dependency: A → B → C → A
```

## Migration from Old Format

Existing skills with `depends_on: ["github"]` continue to work — they're treated as `version: "*", type: skill`. To add version constraints, switch to the dict format:

```yaml
# Before
depends_on:
  - github

# After
depends_on:
  - name: github
    version: ">=1.0"
    type: skill
```

## Architecture

```
SkillManifest.depends_on: list[Dependency]
         │
         ▼
  DependencyResolver.resolve()
         │
         ├── Version check (VersionConstraint.matches)
         ├── Cycle detection (DFS)
         ├── Conflict detection
         └── Topological sort (Kahn's algorithm)
         │
         ▼
  ResolveResult(success, ordered_deps, conflicts, missing)
```

Key files:
- `rust/crates/runtime/src/skills/version.rs` — Version parsing and constraint matching
- `rust/crates/runtime/src/skills/manifest.rs` — SkillManifest with Dependency type
- `rust/crates/runtime/src/skills/loader.rs` — SKILL.md frontmatter parsing
- `rust/crates/runtime/src/skills/registry.rs` — UnifiedSkillRegistry (discovery + resolution)
