# Skill Dependency Declarations

`SKILL.md` frontmatter is the canonical descriptor for a filesystem skill.
Dependencies record the skill or tool versions that an invocation expects.

```yaml
---
name: my-skill
version: "1.0.0"
depends_on:
  - name: git-status
    version: ">=1.0,<2.0"
    type: tool
  - name: knowledge
    version: "~=2.1.0"
    type: skill
---
```

Supported constraints are `>=`, `>`, `<=`, `<`, `==`, `!=`, compatible
release (`~=`), exact bare versions, and `*`. A string entry remains accepted
as shorthand for an unconstrained skill dependency:

```yaml
depends_on:
  - knowledge
```

The parser rejects malformed names, versions, and dependency types. The
canonical implementation is:

- `crates/astra-skills/src/loader.rs` — `SKILL.md` parsing.
- `crates/astra-skills/src/manifest.rs` — `SkillManifest.dependencies`.
- `crates/astra-skills/src/version.rs` — versions, constraints, and dependency
  value types.

## Current execution contract

`/skill install` recursively installs declared skill dependencies, skips an
already available compatible version, and bounds recursion depth. Tool
dependencies are metadata; the installer does not install tools.

Dependency installation is not an atomic graph transaction. Upgrade,
rollback, and uninstall do not currently perform reverse-dependency checks.
Callers must not infer that an accepted declaration proves the complete graph
is executable. Those guarantees require a product-wired installer transaction
and tests through the public command and marketplace boundary; a standalone
resolver with unit tests is not such a guarantee.

Do not add a parallel `manifest.yaml` or `metadata.json` merely to repeat
`SKILL.md` fields. A `manifest.yaml` is read only for the optional
`mcp_servers` CLI extension; skill identity, instructions, dependencies, and
tool permissions remain owned by `SKILL.md`.
