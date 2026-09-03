# Repository Automation

This guide records the maintainer-owned GitHub settings that cannot be fully
expressed in the repository. The versioned configuration in
[`.github/mergify.yml`](../../.github/mergify.yml) remains the source of truth
for merge behavior.

## Merge queue contract

Astra uses a serial, in-place Mergify queue with GitHub's strict branch
protection enabled. A pull request enters the queue only after the required
checks and owner approval pass. Mergify then updates it with the current
`main`, reruns the required checks on that exact head, and squash-merges it.

The queue settings are explicit rather than relying on service defaults:

- `batch_size: 1`, `max_checks_retries: 0`, `mode: serial`, and
  `max_parallel_checks: 1` keep validation on the original pull request.
- `update_method: merge` works for branches in forks without rewriting their
  history.
- Branch-protection requirements are injected by Mergify, so the required
  check list remains owned by GitHub.

Do not add a separate rule that automatically updates every open pull request.
The queue already updates an approved pull request when necessary. Updating
all stale branches creates avoidable CI runs and writes to contributor forks.

## Required GitHub App access

The Mergify installation on `matrixorigin/Astra` must retain these repository
permissions:

| Permission | Access | Why |
| --- | --- | --- |
| Contents | Read and write | Update the queued branch and merge the pull request |
| Pull requests | Read and write | Inspect approvals and manage queue state |
| Checks | Read and write | Publish queue and rule results |
| Workflows | Read and write | Update a Git ref when the incoming `main` commits include `.github/workflows/*` |
| Administration | Read | Read branch-protection requirements |

`Workflows: read and write` does not make workflow changes part of a
documentation pull request. GitHub checks the complete ref update: if `main`
advanced through a workflow change while a pull request was open, merging
`main` into that pull request also carries the workflow commit into its head.
GitHub rejects that ref update unless the app has the Workflows permission.

When Mergify requests new permissions, an organization owner must approve the
installation update. Selecting the permission in the app UI is not sufficient
until the organization installation reports the new effective access.

## Protection settings

Keep the following `main` branch-protection invariants aligned with the queue:

- pull requests and the configured approving review are required;
- required status checks are enabled;
- **Require branches to be up to date before merging** remains enabled;
- direct pushes are restricted, with Mergify as the automation app allowed to
  complete reviewed queue merges;
- administrators do not use bypass as the normal delivery path.

After changing the Mergify installation or branch protection, verify the
effective state rather than relying on the settings form:

```bash
gh api repos/matrixorigin/Astra/branches/main/protection \
  --jq '{strict: .required_status_checks.strict,
         mergify: [.restrictions.apps[] | select(.slug == "mergify") |
                    .permissions]}'
```

The result must show `strict: true` and `workflows: "write"`. Requeue one
approved, behind pull request after any permission change to exercise the same
path external contributors use.

## Failure diagnosis

If a queue entry says that the pull request cannot be updated, classify it
before rerunning CI:

1. A message naming `.github/workflows/...` and missing `workflows` permission
   is an installation-permission failure, not a test failure.
2. A merge conflict requires the contributor or maintainer to resolve the
   branch; increasing app permissions does not help.
3. A required check failure after Mergify updates the branch is a real CI
   result on the current `main` integration and should be debugged normally.

This separation prevents permission failures from being treated as flaky CI
and keeps external-fork behavior part of the repository's delivery contract.
