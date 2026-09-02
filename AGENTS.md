# Repository guidance for coding agents

These instructions apply to the entire Astra repository.

## Start with the source of truth

- Read [CONTRIBUTING.md](CONTRIBUTING.md) before making a change.
- Use the [architecture](docs/design/ARCHITECTURE.md) and
  [design index](docs/design/README.md) to find the contract that owns the
  behavior you are changing.
- Design documents may describe target behavior ahead of an implementation.
  Current code, contract tests, and runtime-profile tests are authoritative for
  what the branch supports today.
- Search for existing owners, implementations, callers, and tests before
  adding a new path.

## Preserve Astra's architecture

- Astra has one agent backbone and multiple capacity providers. CLI, Server,
  Web, User Runner, and MCP must not grow separate lifecycle, context, policy,
  trace, reflection, checkpoint, or audit semantics.
- Keep user-local files, shell, Git, credentials, and private-network access on
  explicitly selected CLI or User Runner boundaries. Server code must not gain
  implicit access to those resources.
- Extend the canonical implementation instead of adding parallel state
  machines, allowlists, storage projections, or compatibility shims.
- Make policy and capability decisions explicit, traceable, and testable.

## Make and verify changes

- Keep changes focused and preserve unrelated work already in the worktree.
- Add or update tests at the narrowest layer that owns the behavior. Exercise
  public entrypoints and unhappy paths when behavior changes.
- Use targeted checks while iterating, then choose the relevant repository
  gate:

```bash
make format-check
make lint
make check
make test-offline
make test-contract
```

- Database, live-provider, and system tests are opt-in. Run them only when the
  change touches that boundary; see the [testing guide](docs/guides/testing.md).
- Update public or design documentation whenever a contract, configuration,
  command, or supported deployment path changes.

## Protect users and repository data

- Never commit credentials, populated `.env` files, private endpoints,
  customer data, proprietary prompts, or unsanitized logs and traces.
- Do not weaken authentication, policy, sandbox, approval, or User Runner
  boundaries to make a test pass.
- Keep pull requests from forks deterministic without repository secrets. Live
  integrations need an offline path or an explicitly optional CI lane.

## Deliver through review

- Use a Conventional Commit-style commit and pull request title.
- For the MatrixOrigin repository, never push directly to its default branch.
  Push a non-default feature branch and open a pull request targeting `main`.
- Before any push, verify the destination remote, source branch, destination
  ref, and remote default branch. Stop if the destination ref is the default
  branch.
