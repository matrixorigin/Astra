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

The policy prioritizes paid GitHub-hosted macOS execution minutes over CI elapsed
time. The static contributor-ID allowlist in `scripts/ci/ci_scope.py` selects the
IDC PR mini. A selected IDC route never becomes a paid hosted route because of
queueing, missing controller, API failure, low disk, toolchain failure, lost agent
or a rerun. Those conditions fail the required checks. Known noneligible PR
authors and default-hosted pushes use one hosted bundle, sharing checkout,
toolchain setup and Cargo artifacts across the native checks. When Mac coverage is
selected, missing or malformed routing metadata selects no Mac and fails the
checks on Linux. A successful scope that selects no Mac coverage skips these
jobs without needing a Mac route. The classifier exposes native Mac scope
separately from Linux CLI scope: `.nvmrc` and `scripts/tui-reflow/**` changes alone
do not start a Mac. Mixed native changes and unknown inputs retain conservative
Mac coverage. Missing native scope fails the gates without allocating a Mac.

Main defaults to hosted. Operators may enable `ASTRA_MACOS_MAIN_ENABLED=1` only
after provisioning a separate isolated `mac-arm64-main` runner. Develop pushes
and release builds stay hosted. The PR runner is `mac-arm64-mini`; never attach
both labels to the same host or share their account, HOME, toolchain, installation,
workspace, registry or caches. Restrict the main runner group to the trusted
`macos-mini.yml@main` worker. Labels alone are not isolation. This PR does not
change runner groups, repository variables, fork approval or branch protection.

The ordinary Test Suite owns the existing required check names. Linux tests
start independently from scope. One successful scope with an explicit hosted
route may allocate a `macos-15` runner; an absent planner/result never authorizes
it. The five existing Mac-facing checks run as lightweight Ubuntu adapters.
One existing coordination job validates the native producer and full Linux
terminal/vendor matrices; the other Mac labels forward that result. Producer
proof must match the current source run ID and tested merge/push SHA, with a
positive producer attempt no later than the current attempt. The canonical owner's summary distinguishes native Mac execution from
portable/alternate-backend Linux coverage. A bundle failure fails all adapters,
including native checks not reached after the first failure. No test is inferred
from a successful checkout or skipped step.

`macos-scheduler.yml` runs trusted default-branch code on Test Suite's
`workflow_run` in-progress/completed events. It never executes candidate code;
its write permission only dispatches/cancels workers. Worker admission verifies
the PR author, head repository, current head, actual two-parent merge, current
source run/attempt/SHA, selected pool and active parent wait step before candidate
checkout. The worker has a read-only token and read-only Actions cache access.
Worker discovery is bounded by this attempt's `run_started_at`, not the run's
original creation time. Delayed completion cleanup reads the historical attempt
endpoint for that same bound; missing timestamps fail closed instead of scanning
unbounded old history. Actual test or compile failures remain failures, even with
incomplete test metadata. Superseded/closed sources request cancellation; late workers reject
ended gates before candidate checkout. Completion cleanup retries exact-source
worker cancellation after control/API outages; cancellation is not atomic with a
concurrent rerun.

An IDC source has 3,600 seconds (60 minutes) for queue/discovery and 3,600 seconds
for execution. The worker's hard timeout remains 65 minutes; parent and scheduler
backstops are 127 and 187 minutes, including delayed gate startup and cleanup.
Gate startup polls every 90 seconds, queued workers every 360 seconds and running
workers every 120 seconds. Slower queued polling bounds API load while allowing
IDC capacity to become available. It can delay queued supersession detection by
one poll plus API latency. API requests retain bounded retries, socket timeouts,
phase deadlines and final observation; cleanup gets a separate 120-second budget.

A one-active/four-queued rolling-hour fixture, counting responses as 200 with 20
startup/admission/write requests per source and no ETag savings, remains below
800 REST requests. This is a bounded workload example, not unlimited capacity or
a guarantee for extra pages, retries and unrelated repository traffic. ETag 304s
reuse only the exact previously verified body and pagination links. Transient
GETs have at most three attempts, respecting Retry-After/reset headers and phase
budgets; dispatch POSTs are not blindly retried. Logs report only numeric quota
headers, never credentials or raw API bodies. Both polling jobs occupy Ubuntu
runners while waiting; this policy trades waiting/control time for paid Mac cost.

After a failed IDC attempt, resolve the runner/control cause and select **Re-run
all jobs** on Test Suite. That schedules a fresh worker for source attempt 2+
while its own worker attempt remains 1. Direct worker reruns stay rejected.
When only Linux work failed, **Re-run failed jobs** can reuse a successful Mac
producer for the same run and SHA. Re-executed gates accept that earlier attempt
only while its selected producer remains successful; different runs, changed
SHAs, invalid/future attempt stamps and failed producers are rejected. New
workers still undergo exact source-attempt admission. The default-branch controller must contain this policy before
rerun-all IDC can be verified; pre-merge tests use the old controller and may fail
closed for source attempts greater than one.

On Darwin, the canonical `scripts/ci/run-macos-lanes.sh all` runs default CLI
PTY/Bash, native Git/workspace coordination with Mac Clippy, and the standalone
Mio simultaneous-readiness test. That test uses Darwin's kqueue, so its exact
inventory must contain one test before execution; zero matches cannot pass.
Linux keeps the full default/dev-tty CLI suites, normal-production-binary reflow
and both complete vendored variants. Mac drops the production CLI binary graph,
alternate-feature CLI graph and bulk portable vendored runs. It needs no Node
setup. Two heavy build graphs plus a small native library replace four heavy
graphs; this is removed work, not a duration estimate.

This is deliberately narrower cross-platform coverage. A Darwin-only problem in
the full production-renderer or non-default dev-tty combination may surface
outside this PR gate. The native default reader, real ToolExecutor Bash,
SIGINT/termios restoration, resize/input, kqueue and workspace behavior remain
validated on Mac; portable full journeys and both readers remain on Linux.
The existing controller step name `Run all macOS lanes` stays stable, so the old
worker can execute the candidate's Darwin-aware dispatcher before merge. It may
still prepare Node until the default-branch worker workflow is updated. The
hosted native bundle has the same 65-minute hard backstop as the IDC worker;
this is a safety bound, not expected runtime.

Self-hosted Rust setup retains Cargo artifacts in
`$RUNNER_WORKSPACE/.astra-ci-target`, outside the checkout that `git clean`
resets. Both terminal-reader feature variants remain available across jobs.
It uses a runner-local sccache capped at 5 GiB, with C/C++ direct mode disabled,
and skips Actions build-cache restore/upload. Cargo still checks source,
dependency and compiler fingerprints on every invocation. These local artifacts
and the Cargo registry belong to the runner's isolated account; PR and main
pools must never share them. Before each run, the disk guard discards the whole
dedicated Cargo directory if it exceeds 64 GiB or free space is below 30 GiB,
then measures free space again. These are job-start thresholds, not a live disk
quota during compilation. Smaller healthy caches remain warm. Cleanup runs
before Cargo on the runner's single worker, touches only that disposable
directory, and does not sweep individual dependency files. If free space is
still insufficient or cannot be measured, setup fails without paid recovery. The separate 5 GiB sccache survives Cargo-directory cleanup.
Hosted runners retain their existing Actions cache path. Compare actual
compile timings and sccache hit rates after warm runs, separately from queue
time; a cold first run still builds the dependencies.

The PR runner must match `[self-hosted, macOS, ARM64, mac-arm64-mini]`; the
isolated main runner uses `mac-arm64-main` instead. Each must be available
to Astra (including public repository access), and support the pinned Actions,
Rust versions. Rustup is bootstrapped if absent, Cargo bin is added to PATH, and the selected
repository toolchain is exported through GITHUB_ENV for later sccache compiler
probes outside the checkout. At least 30 GiB workspace disk is required. On self-hosted runners the
disk guard reports low/unreadable disk without privileged hosted-image cleanup;
the required checks fail until operators restore mini capacity.

One runner registration executes one bundle at a time. Measure hosted job
execution minutes summed across runners, IDC duration, queue time and API load
separately. Queue time is not execution savings. Consolidation removes repeated
preparation/build work, but actual hosted minutes require comparable CI samples.
Rollback is an explicitly reviewed routing change, not an automatic paid retry.
