# Claude Code guidance

Read [`AGENTS.md`](AGENTS.md) in full before changing this repository. It is
the canonical source for Astra's architecture, safety, verification, and Git
delivery rules. Follow [`CONTRIBUTING.md`](CONTRIBUTING.md) for the contributor
workflow and use the [design index](docs/design/README.md) to find the contract
that owns the behavior being changed.

## Claude-specific integration

- Project skills live under [`.claude/skills/`](.claude/skills/). Read the
  matching `SKILL.md` before performing a task covered by one of those skills.
- [`.agent/skills/`](.agent/skills/) is the compatibility mirror used by other
  Agent Skills hosts. Do not update only one copy; the skill contract tests
  require their instruction bodies and manifests to remain equivalent, except
  for explicitly supported host-tool differences.
- Prefer the narrowest relevant `make` target while iterating. Live database,
  provider, and system lanes are opt-in as described in the
  [testing guide](docs/guides/testing.md).

Do not duplicate repository-wide rules here. Update `AGENTS.md` when the
canonical guidance changes.
