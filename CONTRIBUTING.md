# Contributing to Astra

Thank you for helping improve Astra. This guide defines the shortest path from
an idea to a reviewable pull request. Detailed setup, testing, and architecture
documentation remains in [`docs/`](docs/README.md).

Participation in this project is governed by our
[Code of Conduct](CODE_OF_CONDUCT.md).

## Before you start

- Search existing issues before opening a new one.
- Use the repository issue templates for bugs, features, documentation, and
  questions.
- Open an issue before substantial behavior or architecture changes so the
  runtime contract and implementation can evolve together.
- Read the relevant contract in [`docs/design/`](docs/design/README.md) before
  changing lifecycle, execution, policy, context, trace, Runner, or storage
  behavior.

## Set up a development environment

The repository pins Rust in [`rust-toolchain.toml`](rust-toolchain.toml) and
Node.js in [`.nvmrc`](.nvmrc).

```bash
cp .models.yaml.example .models.yaml
make dev-init
# Configure embeddings in .env and a model in .models.yaml.
make build-cli-debug
make dev-start
make dev-status
```

The default is deliberately Server-only. Bootstrap/login with
`./target/debug/astra` before the first Edge start, then use
`make dev-start-server-edge` when Web or Server tests need local files, shell,
Git, or private-network access through the User Runner.

See the [developer setup](docs/quickstart/development.md) and
[development workflow](docs/guides/development-workflow.md) for prerequisites,
runtime profiles, and troubleshooting.

## Make a focused change

- Keep one canonical implementation for each behavior; remove superseded
  paths rather than adding a second source of truth.
- Preserve explicit boundaries between the agent kernel, Server, CLI, and
  User Runner.
- Add or update tests at the narrowest layer that owns the behavior.
- Update design or reference documentation when a public contract changes.
- Never commit credentials, local `.env` files, generated build output, or
  benchmark data containing private inputs.

## Validate the change

Run the smallest relevant check while iterating, then the appropriate broader
gate before opening a pull request. `make test` includes online lanes and is not
the default inner-loop command:

```bash
make format-check       # Rust formatting
make lint               # clippy with warnings denied
make check              # repository static checks
make test-offline       # offline Rust, SDK, Web, and profile tests
make test-contract      # HTTP, admin, and configuration contracts
```

Database, live-provider, and system tests are opt-in. Run them only when the
change touches their boundary; see the [testing guide](docs/guides/testing.md)
for the correct lane. Pull requests from forks must remain testable without
repository secrets, so live-provider coverage must have a deterministic
offline path or stay in an explicitly optional lane.

CI routes required checks by the files changed in a pull request:

| Change area | Heavy checks selected |
| --- | --- |
| Documentation and repository governance | Lightweight repository contracts only |
| Rust crates | Workspace static checks plus affected offline shards; shared runtime paths also select online tests |
| `packages/sdk/` | SDK and Web (the local SDK consumer) |
| `web/` | Web |
| Terminal-Bench harness scripts | Harness contracts |
| CI routing, workflows, or an unavailable diff | All checks (fail-safe) |

Required check names remain present when their heavy work is skipped, so this
routing is compatible with branch protection and the merge queue. The routing
contract and its tests live in [`scripts/ci/`](scripts/ci/).

For fork pull requests, an update may require maintainer approval before the
replacement test run can enter the normal concurrency group. A separate
trusted-workflow-revision controller cancels active runs for earlier heads
without waiting for replacement-run approval. The controller never checks out
or executes pull request code; the
test workflows remain low-privilege `pull_request` workflows. Normal concurrency
groups use the pull request number, so identically named branches in different
forks remain isolated. Each synchronize event cancels only its exact `before`
head and is intentionally not coalesced, so rapid pushes cannot make an older
controller cancel a newer generation or skip an intermediate cleanup. Jobs that
must run after scope-classification failure use `!cancelled()` rather than
`always()`, preserving fail-safe coverage without making superseded work resist
cancellation.

## Open a pull request

1. Rebase the feature branch on the current `main` branch.
2. Use a Conventional Commit-style PR title, for example
   `fix(runtime): preserve trace ordering after reconnect`.
3. Complete the pull request template, including architecture delta, public
   entrypoint, unhappy paths, and database verification where relevant.
4. Link the issue and describe user-visible behavior and compatibility impact.
5. Wait for required CI and code-owner review before merging.

By contributing, you agree that your contribution is licensed under the
[Apache License 2.0](LICENSE).
