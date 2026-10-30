# Repository Automation

This guide records the maintainer-owned GitHub settings that cannot be fully
expressed in the repository. The versioned configuration in
[`.github/mergify.yml`](../../.github/mergify.yml) remains the source of truth
for merge behavior.

## Merge queue contract

Astra uses a serial, in-place Mergify queue with GitHub's strict branch
protection enabled. A pull request enters the queue only after the required
checks and owner approval pass. If `main` advanced, Mergify updates the pull
request and waits for the required checks on the resulting head. If it is
already current, Mergify can reuse its passed checks before squash-merging it.

The queue settings are explicit rather than relying on service defaults:

- `batch_size: 1`, `max_checks_retries: 0`, `mode: serial`, and
  `max_parallel_checks: 1` keep validation on the original pull request.
- `update_method: merge` updates eligible fork branches without rewriting
  their history.
- `branch_protection_injection_mode: queue` makes Mergify enforce GitHub's
  branch-protection requirements when a pull request enters the queue and when
  it merges, while the required check list remains owned by GitHub.

Do not add a separate rule that automatically updates every open pull request.
The queue already updates an approved pull request when necessary. Updating
all stale branches creates avoidable CI runs and writes to contributor forks.

## Required GitHub App access

Mergify requests a fixed permission set for its products. Do not trim that set
to this table: these are the permissions most directly involved in Astra's
queue path. The installation must include `matrixorigin/Astra` in its selected
repository access—or cover all organization repositories—and retain Mergify's
requested permissions, including:

| Permission | Access | Why |
| --- | --- | --- |
| Contents | Read and write | Update the queued branch and merge the pull request |
| Pull requests | Read and write | Inspect approvals and manage queue state |
| Checks | Read and write | Publish queue and rule results |
| Workflows | Read and write | Update a Git ref when the incoming `main` commits include `.github/workflows/*` |
| Administration | Read | Read branch-protection requirements |

The current full permission set and its feature-level rationale are maintained
in [Mergify's security reference](https://docs.mergify.com/security/#github-app-required-permissions).

`Workflows: read and write` does not make workflow changes part of a
documentation pull request. GitHub checks the complete ref update: if `main`
advanced through a workflow change while a pull request was open, merging
`main` into that pull request also carries the workflow commit into its head.
GitHub rejects that ref update unless the app has the Workflows permission.

When Mergify requests new permissions, an organization owner must approve the
installation update. Selecting the permission in the app UI is not sufficient
until the organization installation reports the new effective access.

For a pull request from a personal fork, the author must also select **Allow
edits from maintainers**. GitHub labels this **Allow edits and access to secrets
by maintainers** when the fork contains Actions workflows. This is the author's
security choice; if it is disabled, ask the author to update the branch instead
of bypassing protection. See [GitHub's fork collaboration
guide](https://docs.github.com/en/pull-requests/how-tos/work-with-forks/allowing-changes-to-a-pull-request-branch-created-from-a-fork).

## Protection settings

Keep the following `main` branch-protection invariants aligned with the queue:

- pull requests and the configured approving review are required;
- required status checks are enabled;
- **Require branches to be up to date before merging** remains enabled;
- direct pushes are restricted, with Mergify as the automation app allowed to
  complete reviewed queue merges;
- administrators do not use bypass as the normal delivery path.

Required check names must not begin with `@`. Mergify uses that prefix for a
GitHub-App-qualified check (`@github-actions/<check-name>`), so a required
workflow job named `@astra/sdk ...` cannot be matched by the injected branch
protection condition even when GitHub reports the check as successful. The SDK
job is therefore named `Astra SDK (typecheck, test+coverage, build)`.

When this name changes, update the required status-check list in `main`
branch protection in the same maintenance window: remove
`@astra/sdk (typecheck, test+coverage, build)` and add
`Astra SDK (typecheck, test+coverage, build)`. Keep the old entry until the new
workflow check has appeared once, then remove it so a missing legacy check does
not block every pull request.

After changing the Mergify installation or branch protection, verify the
effective state rather than relying on the settings form:

```bash
gh api repos/matrixorigin/Astra/branches/main/protection \
  --jq '{strict: .required_status_checks.strict,
         mergify: [.restrictions.apps[] | select(.slug == "mergify") |
                    .permissions]}'
```

The result must show `strict: true`; the Mergify permissions must include
`administration: "read"` plus `checks`, `contents`, `pull_requests`, and
`workflows` set to `"write"`. Requeue one approved, behind pull request after
any permission change to exercise the same path external contributors use.

## Failure diagnosis

If a queue entry says that the pull request cannot be updated, classify it
before rerunning CI:

1. A message naming `.github/workflows/...` and missing `workflows` permission
   is an installation-permission failure, not a test failure.
2. A message saying the fork branch cannot be updated without a missing app
   permission can mean maintainer edits are disabled; the author must update
   the branch or opt in.
3. A merge conflict requires the contributor or maintainer to resolve the
   branch; increasing app permissions does not help.
4. A required check failure after Mergify updates the branch is a real CI
   result on the current `main` integration and should be debugged normally.

This separation prevents permission failures from being treated as flaky CI
and keeps external-fork behavior part of the repository's delivery contract.

## Self-hosted macOS tests

The static author-ID allowlist in `scripts/ci/ci_scope.py` makes selected
contributors eligible for the PR mini. A `main` push may use a separate,
dedicated `mac-arm64-main` runner; `develop` pushes and releases stay hosted.
The main runner must be on a physically separate host or a separately isolated,
disposable machine with its own account, HOME, toolchain, runner installation,
workspace and caches. Never attach both `mac-arm64-main` and `mac-arm64-mini`
to the same host or share their writable storage. Restrict the main runner group
to this trusted default-branch worker workflow; do not allow PR-controlled
workflows to use it. Labels alone do not provide isolation. Until operators
provision and validate this boundary, leave the repository variable
`ASTRA_MACOS_MAIN_ENABLED` unset: main runs GitHub-hosted tests immediately.
After provisioning, set it to `1`; an unavailable runner then uses the existing
bounded queue and hosted fallback. PR and main
admission verifies the source event and selected pool before candidate checkout.
Other or unknown PR authors run on hosted macOS immediately; GitHub's existing fork approval remains in effect. Every Mac
job re-executed on attempts greater than one runs hosted, even when its planner's
old output is reused. GitHub may retain successful Mac jobs on a failed-only
rerun; those checks still refer to the original tested merge. Required-check names retain
`macos-15` for compatibility; job summaries link the actual worker execution.

The ordinary Test Suite owns all required checks. Linux jobs depend only on
scope and start without waiting for the mini. `Resolve macOS execution` has
Actions/Pull requests read access and waits for a worker bound to its exact run
ID, attempt and tested SHA (the PR merge or main push commit). Missing
controller, API errors or missing routing output select real hosted tests,
never a successful skip. GETs use authenticated ETag revalidation when the
server supplies an ETag. A 304 reuses only the exact previously fetched body
and pagination links; network/API failures never authorize a stale cached result.
GitHub does not charge correctly authenticated 304 responses against the primary
rate limit. This is an optimization, not a prerequisite for the request budget.

Transient GET failures (network/timeouts, connection resets, incomplete response
bodies and 5xx) get at most three attempts. Rate-limit 403/429 responses respect
Retry-After and primary reset headers; secondary limits without timing headers
back off for at least 60 seconds and then exponentially. Permission-denied 403s
and dispatch POSTs are not retried. Backoff is bounded by the remaining queue
or execution budget, and initial source validation counts toward the queue
budget. Each phase permits a final observation, with API requests starting no
later than 60 seconds after its deadline; this does not permit retry backoff
beyond the phase deadline. In-flight requests retain their socket timeout.
Late observation of an assignment cannot extend the execution deadline beyond
queue deadline plus 60 minutes. Cleanup has a separate 120-second request budget,
without observation grace. Thus a short API outage can recover without restarting healthy tests,
but an outage that outlasts the budget can still require hosted recovery.
Source freshness is checked again before publishing a worker outcome after
potentially delayed reads. If that check is unavailable, a confirmed test failure
stays failed; success still requires fresh validation.

The first response with valid X-RateLimit-Limit/Remaining/Reset headers records
only those numeric values in the job log and summary, using the actual workflow
token. It never logs raw headers, tokens or response bodies. The default quota
is 1,000 requests/hour per repository shared across workflows (15,000 for
Enterprise Cloud), not an allowance for each PR. Do not infer this quota from
a developer's PAT. See GitHub's [rate limits](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api)
and [conditional-request guidance](https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api).

PR admission verifies the merge's actual two parents, with the PR head as its
second parent and its first parent equal to or an ancestor of the current
target branch tip. The event's base SHA may be stale and is not the authority
for this check. The PR head repository and current author ID are still verified.
Workers execute the exact
validated SHA, not a moving branch checkout.

Both readers recheck current PR/head status while queued/running.
A changed head or closed PR, or ended source attempt produces
`obsolete`, not `hosted`. The trusted scheduler requests cancellation of that
source attempt and its owned worker; it rechecks the attempt before cancelling
and skips a newer attempt already visible at that check. Cancellation is best
effort: GitHub's run-ID-scoped API cannot atomically exclude a rerun that starts
between the check and cancel request. If cancellation/control access
is unavailable, native Mac result gates run on Ubuntu and fail on `obsolete`;
they neither allocate hosted Macs nor report missing tests as successful.
An unexpected repository mismatch remains an unavailable-source error, not an
assumed obsolete PR. Initial source validation and worker admission also apply
these rules, covering PR updates before a worker starts.

`macos-scheduler.yml` runs on the default branch on Test Suite's `workflow_run`
start/completion events for PRs and main pushes. It uses the built-in token's Actions write permission
to dispatch/cancel `macos-mini.yml`; it never checks out PR code. No App,
organization membership query, additional secret, custom required context or
admission hook is needed. These workflows must exist on main before dispatch
works. Before that, Test Suite falls back to hosted execution.

A mini attempt gets a 900-second (15-minute) queue/discovery budget, with bounded API
requests, then falls back to the original hosted jobs in parallel. The
reader/controller enforce a 60-minute execution budget even if the runner
disappears; the worker has a separate 65-minute hard backstop. Thus single-machine
unavailability does not hold a
required check for GitHub's 24-hour queue limit. API/control outages can prevent
immediate child cancellation; the completion observer retries cleanup, and a
late worker rejects an ended parent gate before candidate checkout.
The parent gate has an 82-minute job timeout; the controller has 100 minutes
to cover delayed gate startup plus queue/execution/cleanup. These are backstops,
not an extension of the worker's 60-minute execution budget.

The worker performs one candidate checkout, toolchain setup and cache restore,
then runs default reader/Bash, reflow, coordination, vendored units and dev-tty
through the same `scripts/ci/run-macos-lanes.sh` used by hosted/Linux jobs. Cargo
invocations remain separate and dev-tty is last. A real compile/test failure is
reported as failure, without hosted retry. Failed runs with incomplete test
metadata also stay failed rather than hiding a possible test failure. Checkout,
low-disk/toolchain setup,
agent cancellation/loss or the execution budget select hosted fallback. The
worker gets a read-only token and cache-mode read; it does not save default-branch Actions/sccache cache entries.

The PR runner must match `[self-hosted, macOS, ARM64, mac-arm64-mini]`; the
isolated main runner uses `mac-arm64-main` instead. Each must be available
to Astra (including public repository access), and support the pinned Actions,
Rust and Node versions. Rustup is bootstrapped if absent, Cargo bin is added to PATH, and the selected
repository toolchain is exported through GITHUB_ENV for later sccache compiler
probes outside the checkout. At least 30 GiB workspace disk is required. On self-hosted runners the
disk guard reports low/unreadable disk without privileged hosted-image cleanup;
the hosted fallback proceeds while operators restore mini capacity.

One registration runs one bundle at a time. Other eligible runs use hosted Macs
when their queue budget expires. Measure mini execution time, fallback rate,
queue delay, API usage and hosted minutes; queued attempts poll every 90 seconds,
running attempts poll every 120 seconds
and filter worker history by source creation time. Bundling reduces repeated
preparation, but
neither creates physical capacity nor guarantees the previous hosted elapsed
time. The initial 60-minute budget needs qualification against actual load.
The one-page fixture for a full 15-minute queue plus one hour of execution
uses fewer than 400 combined reader/controller REST reads, excluding transient
retries and extra pages. A rolling-hour fixture with one active and four queued
sources, charging every response as 200 plus 20 startup/admission/write requests
per source, stays below 800 requests. This leaves some repository quota headroom;
it is not a guarantee for unlimited concurrency, extra pages, retries or unrelated
workflow traffic. Measure real headers and fallback rates under load rather than
assuming an Enterprise Cloud quota. Both polling jobs occupy Ubuntu
runners while waiting; this change does not remove that control-plane cost.

If the old App/controller design was staged, retire its unused App required
contexts and admission hook, and permit the new `macos-mini.yml@main` worker in
the runner group. Preserve existing GitHub Actions checks and fork approval.
Rollback through a PR by routing all events to hosted macOS; existing reruns
already take that recovery path. No remote runner/group or protection settings
are changed by the implementation PR.
