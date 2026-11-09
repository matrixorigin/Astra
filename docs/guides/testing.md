# Testing Guide

This repository is validated through Rust-first checks, contract tests (fast, many with stub services), and optional **live MatrixOne** system E2E.

## Primary Commands

```bash
# Full suite: workspace + e2e-hooks + online #[ignore] Matrix E2E + multi-agent integration (requires MatrixOne and Memoria for the online portion)
make test

# Workspace + server E2E hooks only (no online #[ignore] suites)
make test-offline

# Deterministic history-copy/serialization accounting (included in test-offline)
make test-history-work

# Online #[ignore] suites only (exports ASTRA_TEST_DB_IT=1; set ASTRA_TEST_DB_IT_TEST_THREADS=1 for serial execution)
make test-online

# Narrow contract smoke (HTTP + admin integration binaries; settings JSON via astra-core lib tests)
make test-contract

# Static validation
make check
make format-check
make lint
make type-check
```

`test-history-work` enables `ASTRA_HISTORY_WORK_TRACE=1` only for the dedicated
`compaction_work` test binary. Its ignored measurements are offline tests, not
live database integration tests; ordinary test processes keep instrumentation
disabled. CI runs this gate in the runtime shard.

Direct `cargo` usage:

```bash
cargo test --manifest-path Cargo.toml -q
cargo check --manifest-path Cargo.toml
```

On a Linux kernel with Landlock enabled, explicitly run the Bash workdir regression:

```bash
cargo test -p astra-tools --lib shell_ops::landlock_tests::bash_workdir_under_landlock -- --ignored
```

It installs a ruleset in an isolated test subprocess, denies reads of the filesystem
root and workspace ancestors, and checks pinned Bash execution, source inspection,
and rejection of outside-workspace access. It fails if Landlock is unavailable;
ordinary offline runs leave this kernel-specific test ignored.

MCP CLI tests launch the prebuilt `mock_mcp_server` beside their test executable.
`make test-offline` builds this fixture before the workspace tests. For direct
`cargo test` or `cargo nextest` invocations that include MCP CLI tests, first run
`cargo build -p astra-cli --bin mock_mcp_server` using the same target directory,
target triple, and profile. Test processes do not run nested Cargo builds.

The provider fixtures and live child-control PTY client use
`astra_core::net::client_builder_for_target` so loopback traffic works with or
without inherited proxies, even when `NO_PROXY` is absent.
Remote API clients retain environment proxy routing; external provider requests
retain the provider proxy policy. Do not globally disable proxies to fix a local
test. Verify both environments in separate processes (including uppercase and
lowercase proxy variables), using an unreachable proxy for the loopback lane:

```bash
cargo test -p astra-core --test proxy_routing
env -u HTTP_PROXY -u http_proxy -u HTTPS_PROXY -u https_proxy \
  -u ALL_PROXY -u all_proxy -u NO_PROXY -u no_proxy \
  cargo test -p astra-runtime --lib server::provider_test_support::
env -u NO_PROXY -u no_proxy HTTP_PROXY=http://127.0.0.1:9 \
  http_proxy=http://127.0.0.1:9 HTTPS_PROXY=http://127.0.0.1:9 \
  https_proxy=http://127.0.0.1:9 ALL_PROXY=http://127.0.0.1:9 \
  all_proxy=http://127.0.0.1:9 \
  cargo test -p astra-runtime --lib server::provider_test_support::
```

Live `astra-test` quality judging invokes `astra session judge --model MODEL
--message JUDGMENT_REQUEST_JSON`. This is one tool-free `VerificationJudge`
completion through the existing authenticated Offering and durable inference
owners. The input is the shared versioned `JudgmentRequest` schema. Ordinary
LLMs return an `answers` map containing one explicitly typed discrete answer
per exact question ID; Noul answers are yes/no/unknown, Choice answers name an
option or use an explicit `null` for unknown, and Score answers name a level or
use an explicit `null`. Omitting the Choice/Score field is malformed, not an
implicit abstention.
TypeSafe returns provider-native judgment values. The CLI returns normalized
answers and execution-owned provenance; answer shape or model-name text cannot
select the decoder. Required `hard_judger` assertions ask one
`criterion_accepted` Noul question: does the evidence meet the criterion's own
rules at its declared threshold? A determined answer is a verdict, not a
numeric grade, and is not compared to that threshold again. Native Noul values
at or below 0.2 mean no, at or above 0.8 mean yes; the middle range and explicit
unknown answers are inconclusive. Quality `judger` criteria retain the four
rubric categories mapped to 1.0, 0.7, 0.4, or 0.0; external command judgers
retain their numeric score protocol and threshold comparison. Provider
probabilities are never evaluation scores. Missing, extra, duplicate, wrongly
typed, uncertain, and conflicting answers fail without format repair.

Quorum keeps numeric aggregation for grades. Acceptance verdicts use strict
majority for median (a tie is inconclusive), all votes for min, and any vote
for max; mean is invalid for verdicts because averaging already-thresholded
answers cannot recover the original grade mean. Required judgments fail if
any vote is missing or errors, rather than discarding that vote. Mixed grades
and verdicts are invalid. Each judgment and quorum vote creates its own
session; its real usage is separate from the measured agent session. The CLI
closes the evaluation session after a completed response or client-error rejection.
Uncertain gateway and transport failures retain the session identity for diagnosis without
automatically retrying. Session closure is not inference cancellation. The
Server owns the provider deadline (`--timeout-seconds`, 1–120 seconds); the
harness subprocess watchdog allows an additional 60 seconds for transport and
session bookkeeping. Failed subprocess stdout and stderr are retained as
bounded diagnostic data in report details, never interpreted as a valid score.
Truncated, filtered, or unknown completion endings are rejected even if their
text contains a score. External `--judger-cmd` process timeout behavior is unchanged.

Live quality judging receives complete invocation-scoped tool documents from
its authorized trace, including identities, dispositions, arguments, results
and Work receipts. Rejected and suppressed requests stay non-executions.
Child-result judging additionally retains the existing Server custody gates.
Tool evidence and ordered user/assistant exchanges are structured input, not
stderr excerpts. The harness does not truncate or repack them to obtain a
verdict: built-in requests use the existing governed Server admission boundary,
while an external command rejects a complete serialized envelope above 64 KiB
before spawning. Stderr remains bounded diagnostic text. Inconclusive judgments
and input-limit failures stay unavailable; they are never converted to passes.

An owned archived invocation can be replayed without another model call using
`ASTRA_TEST_JUDGER_REPLAY=/path/to/fixture.json cargo test -p astra-test-harness
recorded_action_documents_reach_actual_judgment_request -- --ignored`.
The private fixture contains `outcome`, machine `stream_capture` identity and
`session` trace; the check compares every complete tool document with the actual
judgment request. Store captured fixtures outside tracked source.

Harness cleanup cancels only the exact server-issued Session observed by its
CLI invocation. `astra session cancel` waits up to 10 seconds for the Server's
`execution_settled: true` proof; a `cancelled` display status without that proof
is not sufficient. Pending cancellation, missing proof, a different Session
identity, or a timeout fails cleanup instead of claiming that the checkout is
free. The CLI returns exit code `7` only for a typed, still-pending cancellation
of the requested Session; the harness retries that result within one bounded
cleanup window and never retries unrelated failures. Deletion after capture is
harness-owned history cleanup, not a requirement
for starting a new conversation in an idle checkout.
An interactive Ctrl-C stops new case admission, kills the active CLI process
group, and uses that same exact-Session cancellation path before returning a
non-passing interrupted result. The case watchdog also covers stdout/stderr
and machine-event drain after the CLI process exits; a descendant holding a
pipe open cannot turn an incomplete capture into a passing result or leave the
harness waiting without a bound.

Execution preflight failures retain only categorical `final_state` and
`interruption_kind` alongside the existing safe stage and error identifiers.
These facts require a strictly parsed CLI terminal matching the process exit;
unknown interruption labels, protocol failures and older sidecars produce null
fields. An interrupted preflight remains a failed gate, with zero cases run.
The report never includes provider text, interruption reasons or raw envelopes.

Live agent cases require exit 0 by default. A negative test can explicitly
expect a nonzero terminal code using `exit_code`, together with the expected
`final_state`, `interruption_kind`, and evidence assertions. Passing means the
negative behavior matched the test; the report retains the actual interrupted
state and nonzero code. Only a passing composite branch containing a matching
`exit_code` can authorize it. Root and follow-up expectations apply to their
own invocations, not another turn's accumulated output. Protocol failures,
missing terminal identity, and outer harness timeouts cannot be accepted this
way. An explicitly expected nonzero code suppresses automatic rate-limit retry;
session identity, durable evidence, and subsystem-health checks still apply.
Before starting a follow-up, the root must have succeeded or already proved its
expected negative terminal; a session ID alone does not authorize continuation.
Journal-dependent negative expectations use the current root run and invocation
time range and honor the configured subsystem settlement budget. After any turn
loses lifecycle validity, the harness records its evidence and cost, stops later
steps, and runs the shared cleanup and teardown. A normal assertion failure or
an explicitly expected negative terminal does not stop the sequence by itself.

Capability probes distinguish a disabled optional capability from an enabled
capability without owner credentials. A child allowlist cannot enable a parent
capability: the disabled GitHub case expects admission rejection before a child
runs. A separate discovery-only case requires a real child and its causally
attributed discovery result. Validate the complete allowlist, not just selected
array positions; parent discovery of the delegation tool is a separate call.
User-required exact replies and requested observations remain quality criteria,
even when capability enforcement itself succeeds.

The Work planning and observation cases make their explicit no-tools constraint
and core semantic requirements hard checks. `text_json_dag.existing_node_ids`
can supply nodes already declared in the input: output nodes cannot redeclare
them, and output edges referencing them participate in the same cycle check.
This field supplies identities only, not existing edges. The DAG check covers
the supplied output edges, so a context with prior dependencies requires those
edges too before claiming the entire combined graph is acyclic.

`journal_work_replacement_lifecycle` checks exact initial, cancelled, added, and
delivered item counts using canonical Work and branch identities. It allows any
initial item without a delivered outcome to be cancelled, requires fresh addition identities, and
checks that the remaining initial items and additions each have a delivered
settlement. Replays of the same execution do not count twice. Its optional
`cancellation_after_deliveries` minimum requires causal evidence that the target
remained active until the preceding deliveries; a late snapshot alone is not
proof of deferred cancellation. Natural requests with unspecified cancellation
targets and requests for an explicit deferred replacement use separate cases. Cancellation permits execution under an earlier active revision;
accepted retirement receipts must fence later execution even before another
complete board arrives. Use `require_cancel_before_execution` when the user
requires the item never to start. Use `require_board_before_execution` to reject
unbound pre-reads and require successful evidence under the delivered assignment.
Parallel batch position alone does not prove cancellation/execution ordering.

The offline `multi_agent_binding_http_e2e` journeys exercise sessionless runtime
execution, real model/skill HTTP, and canonical SSE projection. They do not
exercise inbound HTTP authentication or persistent Session creation/admission;
those boundaries require the real database consumer journey below.

## MOI consumer compatibility with a fresh Astra database

`Test: MOI fresh-Astra compatibility` starts real MatrixOne and the real Astra
HTTP router over a newly created, uniquely named database. The existing required
`Test: online (core)` check requires this job to succeed; cancellation, failure,
or skipping it cannot produce a green aggregate. It runs independently of path
classification and needs no repository secrets or external model service.

The consumer fixture in `crates/runtime/tests/moi_fresh_astra.rs` and its
`normal_chat.rs` module share one baseline: Matrixflow `dev` commit
`c244138ec330b768e7fb6ff8bbbc97f2d91081aa`, recorded in `MOI_CONTRACT_COMMIT`.
The source contracts are:

- `moi/pkg/service/agentresource/astra_protocol_astra_runtime_backend.go`
- `moi/pkg/service/agentresource/astra_runtime_start_recovery.go`
- `moi/pkg/service/agentresource/astra_runtime_history_recovery.go`
- `moi/pkg/service/agentresource/astra_protocol_astra_runtime_recovery.go`
- `moi/pkg/service/agentresource/astra_protocol_astra_runtime_session.go`
- `moi/pkg/service/agentresource/agent_runtime_binding_reconcile.go`
- `moi/pkg/components/astrabinding/registrar.go`
- `moi/pkg/components/astraauth/provider_request.go`
- `moi/pkg/api/handlers/agent_runtime_gateway.go`

Requests, HMAC signing and response parsing intentionally do not use Astra's
request/response structs. Missing-resource errors come from the real empty
database, not a mock. The mock represents retained MOI data and its bounded
pre-run repair policy: one session repair and one full binding-set repair, only
for the exact missing-resource codes and identities, never after run allocation.
Changing Astra's API must not be accompanied by silently updating this frozen
consumer to make a breaking change pass. CI does not fetch a moving MOI branch.
To advance the baseline, review the MOI changes from the pinned commit, update
the shared fixture and affected assertions, and record the new commit here and
in `MOI_CONTRACT_COMMIT`. A pass applies to that consumer baseline, not an older
MOI deployment automatically.

The two journeys cover an existing conversation and an existing agent opening
a new conversation. They verify idempotent session/binding registration,
foundation plus extension bindings, historical text/file references reaching
the model, discovery snapshots and callback discovery, authenticated Skill/MCP
callbacks, successful SSE termination and a subsequent turn reusing context
without repeating model/tool work. A deterministic loopback model checks its
actual inputs; canned answer text alone is not accepted as proof of restoration.

The same required job also runs `moi_fresh_astra/normal_chat.rs` through that
consumer and real router/database. The pinned consumer accepts
`cancellation_requested`; the test still requires actual `cancelled` settlement
before reusing the session. It checks:

- reasoning and text arrive before the model finishes (a controlled upstream
  stream barrier, not timing assumptions), with complete ordered output;
- Skill/MCP start and result events retain their tool/call identities;
- successful `run_finished` is followed by authoritative `turn_complete`,
  agreeing with the streamed answer, before EOF;
- an active session rejects a concurrent turn with its structured conflict;
- the signed `DELETE /chat/runs/{run_id}` cancels a live run, preserves an
  already completed run, and reports `run_not_found` for a lost historical run;
- the negotiated `x-moi-model-gateway-error-contract: v1` payment rejection
  preserves the error code, HTTP status, action and retryability in a correlated
  failure and failed terminal, without a successful answer or provider-body leak;
- the same session accepts subsequent turns after cancellation and failure.

Scripted providers have a finite response inventory: duplicate inference or an
unexpected call fails the test rather than receiving a generic mock response.

The same fresh-database journey also exercises the additive reasoning-level
contract in `moi_fresh_astra/reasoning.rs`, pinned separately to Matrixflow
`2403ed2b49e450146465a09a931515c4b772d8f6`. That producer freezes the selected
catalog model's supported efforts into
`capability_descriptors.model_gateway.metadata.reasoning_control`, independently
of the requested `context.thinking`. The historical consumer baseline above is
unchanged. This module reuses its independent HMAC signer and SSE parser, without
using Astra request types or fetching a moving MOI branch. It checks:

- `low`, `medium`, `high`, and `max` arrive unchanged in the actual model request
  and each turn completes on the same persisted session;
- a subsequent ordinary turn omits `reasoning_effort` rather than retaining the
  preceding explicit level;
- an undeclared effort, missing capability, or malformed declaration is rejected
  before run allocation or model I/O, with stable SSE error classification;
- a valid turn still completes on that session after the rejected requests,
  without duplicate inference.

The additive `moi_fresh_astra/forks.rs` contract uses the same independent
signer and real router, with its own `astra_test_moi_fork_<uuid>` database.
It exercises signed prepare, continuation activation, and exact committed
root-run cursor lookup, then continues the parent, two siblings, and a nested
fork through ordinary chat. Signature path and replay controls use the supported
cursor route; legacy list/read/writer-activation routes retain their
existing authentication rather than admitting request-signed provider tokens.
The finite model inventory checks the actual inherited input and excludes later
parent or sibling turns. Activation replay must preserve a continued child's
head; activation alone grants no writer lease and makes no inference call.
Negative controls cover signature/body/path tampering, nonce replay, workspace
scope isolation, idempotency conflicts, non-root runs, incomplete projection,
and unavailable historical roots. The earlier consumer baseline is unchanged.
The same fork journey deletes its middle Session, then its ancestor, and proves
the descendant can still materialize and continue from the exact pinned history
after bounded cleanup. Deleted Sessions retain no live head or execution state.
Deleting the final descendant releases fork metadata, pins and manifest rows,
including the node-local payloads stored by the current canonical owner.

`moi_fresh_astra/fork_auth_compat.rs` checks that real runtime and Edge user
tokens retain the existing prepare and writer-activation contract at a
canonical head before Server projection. These legacy credentials cannot use
the new provider cursor or continuation-activation controls. A custom
request-only provider authenticator without mechanism evidence cannot bypass
scope or retry legacy authentication. The canonical verifier also proves the
legacy/HMAC mechanism distinction and single nonce consumption.

MOI's new provider fork integration uses request-bound HMAC authentication.
Those principals own Sessions under `provider_authorized:<provider>:<subject>`;
legacy MOI user tokens use the separate `external_authorized:moi:<subject>`
owner namespace and retain their existing subject-owned Session contract.
Legacy credentials cannot read or control a request-signed provider's fork.
This integration does not change legacy APIs into workspace-scoped APIs.

New request-signed provider Sessions and forks enforce scope at common Run
admission and access. Existing ordinary Sessions without a scope binding retain
their previous chat behavior, but cannot serve as a source for a new provider
fork. A later Run never claims that Session's historical scope. A malformed
binding or an existing fork manifest cannot use the ordinary-session fallback.
The fresh-database fixtures do not establish an existing-database migration or
repair older ordinary Sessions' pre-existing scope behavior.

Buffered and live consumers share the pinned MOI SSE framing rules, including
LF/CRLF, multiline data, EOF frames and stopping at `[DONE]`. Negative controls
reject an early `[DONE]` even when a terminal follows it. MCP and Skill callbacks
use distinct routes and validate JSON-RPC 2.0, route/method admission and the
runtime grant; invalid callbacks fail the journey even if Astra retries. The
same CI command includes these parser/admission controls and the database journey.

This is an interaction contract, not an old-Astra-schema migration test, an
in-flight upgrade test, or an LLM quality benchmark. It protects these concrete
consumer journeys, not every possible MOI feature.

To run against a **disposable local MatrixOne**, supply the normal explicit
`MATRIXONE_*`, `ASTRA_JWT_SECRET`, `ASTRA_TOKEN_ENCRYPTION_KEY`,
`ASTRA_RUNTIME_ROOT_SECRET` and `MEMORIA_MASTER_KEY` test settings, then:

```bash
ASTRA_TEST_DB_IT=1 RUST_MIN_STACK=16777216 \
cargo test --locked -p astra-runtime --features moi-compat-tests \
  --test moi_fresh_astra -- --include-ignored --nocapture
```

No Memoria process is needed. The test creates and drops only its own
`astra_test_moi_<uuid>` database. A failed run can leave that diagnostic database
behind; CI disposes of the entire isolated dependency stack. The dedicated
feature keeps this test out of generic offline/online lanes, avoiding duplicate
full-stack execution.

## Candidate CLI and Edge integration

The shared CLI CI job exercises the current CLI and standalone Edge against a
fresh production Server backed by disposable MatrixOne and Memoria. The driver
uses an isolated HOME, a synthetic account and a controlled model provider. It
checks model discovery, ordinary chat, approval and denial, write readback,
cancellation with and without committed history, same-session resume, and an
Edge WebSocket tool invocation. Missing clients or failed scenarios fail the job.
The separate MOI consumer compatibility gate is unchanged.

The PR integration report records binary hashes, Server source, scenario results,
and bounded, redacted error categories, without credentials or raw transcripts.
It uses debug clients and a Server with test features, while release images use
release builds with default features disabled.
Public and IDC candidate-image smoke jobs separately verify image identity,
health, and a Memoria round trip; they do not run the CLI or Edge journeys above.
In particular, the PR integration does not prove the behavior of an independently
selected IDC source or of the final release binaries. Deployment uses a fresh
database and local state and does not promise compatibility with older clients.

## Where Tests Live

- `crates/runtime/tests/` — HTTP integration tests for `astra-runtime` (including `*_contract.rs`, `system_matrix_http_e2e/`, bridge E2E).
- `crates/services/tests/` — service-layer tests (e.g. `multi_agent_integration` with live DB when `ASTRA_TEST_DB_IT=1`).
- `fixtures/contracts/` — JSON fixtures for contract tests that load shared request/response shapes.
- Capability ↔ route ↔ E2E mapping: [`docs/testing/system-e2e-matrix.md`](../testing/system-e2e-matrix.md).
- SaaS capability test plan: [`docs/testing/saas-test-plan.md`](../testing/saas-test-plan.md) (`make test-saas`; Rust HTTP E2E plus optional remote `@astra/sdk` coverage).
- Coverage matrix (what replaced stub tests, large-binary audit): [`docs/testing/coverage-matrix.md`](../testing/coverage-matrix.md).

The opt-in live Work PTY journey uses ordinary multiagent conversation. Set
`ASTRA_TUI_LIVE_API_URL`, `ASTRA_TUI_LIVE_MODEL`, and
`ASTRA_TUI_LIVE_ACCESS_TOKEN` for an isolated Server and account with a
configured real provider. Two child agents produce a customer master and
dependent invoice exceptions through actual Edge file reads and writes.
Successful launch receipts, durable child identities and typed Explain
terminals must agree; every invoice child must follow an observed completed
customer-master child, including corrective runs. Root Work receipts and one
task-graph read per round verify two dependent primary assignments and exact
delivered settlements. After a CLI restart and `/resume`, ordinary input
changes the duplicate rule: Work, branch and task identities stay stable,
revisions increase, and attempts change through an accepted graph proposal.
Both versions' JSON artifacts are checked exactly. Session audit must retain
physical-attempt usage and either a finite cost estimate or an explicit
unavailable reason. API reads verify execution; the TUI creates the Work and
launches the children. This journey covers revision between settled runs,
not recovery of an active run or transfer to another Edge.

```bash
cargo test -p astra-cli --test tui_pty_journey \
  live_agent_delivers_dependent_work_items_and_reworks_after_client_restart \
  -- --ignored --exact
```

The opt-in live child-control PTY journey uses ordinary conversation and
an explicitly requested child model. Set `ASTRA_TUI_LIVE_API_URL`,
`ASTRA_TUI_LIVE_MODEL`, `ASTRA_TUI_LIVE_MEMBER_MODEL` and
`ASTRA_TUI_LIVE_ACCESS_TOKEN` for an isolated Server and account. The oracle
checks the actual child Offering, same-run pause/guidance/resume, successful
fresh Edge reads and the parent's final adopted answer. API reads verify
execution; they do not create or substitute a child launch.

```bash
cargo test -p astra-cli --test tui_pty_journey \
  live_child_pause_guidance_resume_preserves_execution_identity \
  -- --ignored --exact
```

## Terminal resize and reflow

The inline TUI regression drives the real binary through a controlling PTY and
xterm.js's headless terminal (including native reflow). It checks repeated
narrow/wide and short/tall transitions, draft input, rapid resizes with delayed
cursor replies, one live footer/composer across the entire buffer, and
preservation of pre-existing and committed startup history. A held reply also
outlasts the real cursor-query deadline: paste must remain responsive, no second
query may overlap the expired request, and releasing its reply must allow a
fresh resize without damaging history. The initialization cursor getter and
resize queries share the same reader and outstanding-request ownership. It uses a
synthetic token and an unavailable loopback endpoint; no model or real account
is needed. Python 3 and the repository's Node.js version are required.

```bash
cargo build --locked -p astra-cli --bin astra
npm ci --ignore-scripts --prefix scripts/tui-reflow
ASTRA_TEST_BINARY="$PWD/target/debug/astra" npm test --prefix scripts/tui-reflow
```

CI runs the default terminal reader, `use-dev-tty` reader and all eight
production-binary reflow journeys in three parallel Linux jobs. Linux also runs
the complete vendored units with both reader feature sets. One Mac bundle runs
the existing default CLI PTY/Bash suite, native workspace coordination/Clippy,
and the single Mio simultaneous-readiness regression on Darwin's kqueue.
The same dispatcher selects this platform-native inventory on Darwin and the
full inventory on Linux. Mac does not prepare Node or build the production CLI
and alternate-feature CLI graphs just to repeat the portable integration matrix.

The existing Mac-named terminal/vendor check names are compatibility gates for
this platform split: they require passing native Mac results and passing Linux
terminal/vendor matrices, and their summaries state what ran on each OS. Failed,
cancelled or unexpectedly skipped required work cannot pass. This intentionally
removes macOS production-binary reflow and dev-tty cross-product coverage; Linux
cannot prove those exact Darwin combinations. Default Mac PTY tests retain real
SIGINT/terminal restoration, input and resize/cursor behavior. Unaffected changes
skip heavy jobs. IDC failure never allocates paid fallback; producer stamps must
match the current run and SHA, with a valid producer attempt no later than the
current attempt. Failed-only reruns can reuse a same-version Mac success after
Linux recovery. Node/reflow-only changes run Linux coverage and skip Mac; mixed
native or unknown changes retain Mac coverage. Pushes populate the terminal
cache once per OS.
The Rust terminal-reader PTY tests also
verify that a resize cursor query preserves keyboard/paste input and times out
when the terminal does not answer.

## Live MatrixOne system E2E

CI runs three independent online jobs: `core-runtime`,
`core-services`, and `integration`. Each runner starts its own MatrixOne and
Memoria stack and keeps `ASTRA_TEST_DB_IT_TEST_THREADS=1`. The core shards use
distinct database suffixes as well. This parallelizes package builds and test
execution without increasing contention on a shared database. The required
`Test: online (core)` check aggregates all selected online jobs; the integration
job retains `Test: online (integration)`.

The integration lane also runs the real Next/browser-session database test. It
installs the Node version pinned in `.nvmrc`, builds the local SDK and Web from
that job's checkout, and installs Playwright's Chromium headless shell. The real
browser checks recovery from an unreadable session, retry and explicit sign-in.
Web, SDK and Node-version changes select this lane as
well as their ordinary static checks; the other online lanes do not install Node.
Before running the integration lane, `make test-online` without a lane, or
`make test-ignored-integration` on a clean local checkout, run:

```bash
nvm use
npm --prefix packages/sdk ci --ignore-scripts
npm --prefix packages/sdk run build
npm --prefix web ci
npm --prefix web run build
(cd web && npm exec -- playwright install --with-deps chromium --only-shell)
```

Missing Web build artifacts fail the test; they do not silently skip coverage.

`make test-online` still runs the complete suite, and `ASTRA_ONLINE_LANE=core`
still runs both core groups. To reproduce a CI shard, choose
`ASTRA_ONLINE_LANE=core-runtime`, `core-services`, or
`integration` and run `make test-online NEXTEST_ONLINE_PROFILE=strict-online-ci`
with `ASTRA_TEST_DB_IT_TEST_THREADS=1`. Parallel CI isolation comes from separate
service stacks; distinct database names alone do not remove shared-host load.
All existing test selections, features, exclusions, and per-test deadlines are
retained.

The integration lane includes `pool_shutdown_db_it`. It forwards a real MySQL
handshake and query, withholds only the returned connection's health-check ping,
and verifies that public `SharedPool::close()` completes and the client socket
closes without a ping response. A second case withholds a queued transaction
rollback, then verifies rollback and key-lock release using an independent
connection. Run these cases against the explicitly configured
disposable MatrixOne instance with:

```bash
ASTRA_TEST_DB_IT=1 cargo nextest run -p astra-runtime --test pool_shutdown_db_it \
  --profile strict-online-ci --run-ignored only
```

Memoria identity/credential fixtures require `ASTRA_TEST_DB_IT=1` and an
explicit `ASTRA_TEST_DATABASE` matching the effective database name. The normal
online runner supplies its isolated lane database; local callers must designate
their disposable database rather than relying on a developer-specific prefix.

Contracts requiring an external scoped-key Memoria API or a real provider are
separately gated by the services `external-contract-tests` feature. They are not
selected by ordinary MatrixOne online lanes. To run the scoped Memoria contract,
provision a test Memoria API with scoped keys, set `ASTRA_TEST_MEMORIA_URL`,
`ASTRA_TEST_MEMORIA_MASTER_KEY`, `ASTRA_TEST_DATABASE` and the test MatrixOne
connection variables, then run `make test-memoria-auth-online-contract`.
Missing configuration fails the explicitly selected contract; it is not reported
as a passing test. No production credentials are needed in fork CI.

The key-free BYOK network smoke is also explicit:

```bash
ASTRA_TEST_BYOK_MODELS_URL=https://api.moonshot.cn/v1/models \
CARGO_INCREMENTAL=0 cargo test --locked -p astra-services \
  --features external-contract-tests --test byok_live_network -- --ignored
```

It expects HTTP 401 with no provider key and proves DNS/TLS/HTTP reachability,
not successful model inference.

The provider-wire regression uses a strict loopback HTTP fixture and a disposable
MatrixOne database, without real provider keys. Create the designated database
first and supply its `MATRIXONE_*` connection settings. The fixture binds an
available loopback port:

```bash
ASTRA_TEST_DB_IT=1 ASTRA_DATABASE_PREFIX= \
ASTRA_DATABASE=astra_test_probe_local ASTRA_TEST_DATABASE=astra_test_probe_local \
CARGO_INCREMENTAL=0 cargo test --locked -p astra-services \
  --features external-contract-tests --test user_model_probe_db_it -- --ignored
```

This covers create, credential rotation, explicit probe and failed-write
preservation. Official OpenAI/Anthropic probe and rotation tests seed only their
fixture rows with loopback endpoints; production official endpoints remain fixed.
The fixture uses the current schema and verifies that credential rotation
invalidates observations. Use only the explicitly designated test database;
never designate a database containing non-test data.
`schema_assertions::core_schema_catalog_matches_live_idempotent_bootstrap`
creates its own disposable database to cover fresh bootstrap, expired lease
recovery, interrupted-bootstrap retry, repeated validation, old-marker rejection,
and failure without readiness publication when a required key is missing.
Bootstrap does not migrate old schemas; recreate an unsupported database.
`memoria_reauthentication_http` separately covers same-key reconnect, pending
proof invalidation and an in-flight verification crossing disconnect/reconnect.

```bash
ASTRA_TEST_DB_IT=1 \
ASTRA_TEST_E2E_SECRET=system-matrix-e2e-secret \
ASTRA_BACKEND_SERVICE_KEY=test-service-key-e2e \
ASTRA_LLM_RETRY_BASE_MS=10 ASTRA_DEFAULT_RETRY_AFTER_MS=10 ASTRA_BCRYPT_COST=4 \
RUST_MIN_STACK=16777216 \
cargo test -p astra-runtime --test system_matrix_http_e2e --features e2e-hooks -- \
  --ignored --nocapture --test-threads=1
```

Run journeys sharing a database serially: every Server bootstrap performs
startup recovery and can fence another fixture's live execution under the same
process owner. Parallel journeys require separate databases, not just distinct
users or sessions. The Phase-0 production baseline journeys have additional
exclusive-run configuration; see their dedicated instructions before enabling
them.

The slow-fanout root-cancellation journey uses the fixture-owned Session, so
`ctx.close()` drains run/finalizer work before deleting its data. It must not
delete another Session before that drain. All cancelled-child replay responses
share one 10-second deadline and retain the 1 MiB body limit. Its
`root_cancel_phase` markers record only phase names and elapsed time, allowing a
CI timeout to distinguish setup, provider entry, cancellation, replay and
teardown without publishing identities or payloads. The suite's 60-second
per-test limit and cancellation/late-output assertions remain in force.

Requires the same environment as `astra-server`: `MATRIXONE_*`,
`ASTRA_JWT_SECRET`, `ASTRA_TOKEN_ENCRYPTION_KEY`, Memoria, and embedding
settings parsed by `astra_core::AppSettings::from_env`. Use a local `.env` if
you use one for development. To isolate from production on one MatrixOne host,
set **`ASTRA_DATABASE_PREFIX`** (effective DB = prefix + `ASTRA_DATABASE`).
Optionally set **`ASTRA_AUTO_CREATE_DATABASE=1`** so the first
`ensure_core_schema` (server or online tests) runs `CREATE DATABASE IF NOT
EXISTS` for that effective name (bootstrap catalog defaults to `mysql`).

### Ordinary-chat SQL budget

`chat_sql_budget::ordinary_chat_and_readonly_rounds_have_bounded_total_sql`
uses the same isolated MatrixOne setup and scripted loopback provider as the
system HTTP journeys. Run this test by exact name in its own process because
it owns the global SQL tracing subscriber:

```bash
MEMORIA_SELF_HOSTED_MASTER_ACCESS=1 \
cargo test -p astra-runtime --features e2e-hooks --test chat_sql_budget \
  ordinary_chat_and_readonly_rounds_have_bounded_total_sql -- --ignored --exact --nocapture
```

The scoped nonperiodic limits are 23 statements for a cold ordinary chat, 24
for a warm chat, 33 for Auto and 20 with observation unconfigured. Warm chat
restores prior canonical Context once in addition to the observer's captured-head
read; the test asserts those reads explicitly. Every SQL statement remains in the raw
total. At most one exact account/session authentication snapshot is allowed;
exact control reads have a separate bound of `ceil(measured_elapsed_ms / 2000)`.
Thus the raw envelope is the core limit plus that snapshot and the elapsed timer
allowance. Extra business SQL, duplicate authentication reads or polling faster
than the two-second contract still fail. This avoids assuming a loaded CI host
will always finish before the second control tick.

These are **scoped request-plus-attempt budgets**, not fleet totals. The Router
fixture drives one Session through the production claim, authority,
materialization, dispatch and receipt path. Pool setup and warm-up finish before
capture; it ends only after the expected success and durable receipt. Denial,
error and dispatch metrics alone cannot pass.

Observation adds no job insertion or synchronous SQL statement to chat settlement.
The existing canonical completion write marks its head due, preserving active
leases and retry deadlines. Changing that due time does update the physical key
and indexes, so equal SQL counts are not a claim of identical database work.
Unconfigured deployments do not poll or dispatch observations, but retain the
canonical due fact for later recovery.

`canonical_observation_burst_recovers_without_chat_job_writes` separately measures
the actual managed loop with two owners, 1,000 canonical Sessions, a held slow
observer, 16 paced arrivals and 4,096 deleted historical identities. Raw active
capture includes **all** observation-recovery statements with the synthetic
observer, receipt verification polls and arrival setup. Authority/HTTP SQL is
covered by the scoped integration probes; unrelated Server maintenance loops
are outside this observation scheduler fixture. It then restarts two idle owners for 5.2 seconds, first with the
observed history retained and then deleted. Observation SQL must contain only due
reads (at most eight total), with no lease writes or transcript reads. The HTTP
fixture's exact 30-second test-account heartbeat is reported separately (at most
one per interval) and remains in the bounded raw total. No SQL is discarded. This makes
idle fleet cost visible independently of the marginal per-Session attempt.
The test first verifies named independent Sessions progress while a call remains
held, then cancels that owner and separately waits for the full backlog and paced
arrivals. Fairness does not require draining a thousand Sessions before the held
request's production deadline. It also verifies the eight-attempt bound,
non-overlapping claims, handoff and every durable receipt before shutdown.

The ignored services test
`session_observation::tests::retained_catalog_plans_keep_payload_reads_selective`
uses the real schema with 100,000 retained heads. Prepared `EXPLAIN ANALYZE`
checks actual storage input for empty and sparse due work, a selected head,
claim/index-maintenance writes and canonical materialization. The due timestamp leads the primary key; a nullable
secondary index was tested and rejected because it scanned retained history.
The full SessionKey remains UNIQUE. The regression also covers a live Session
with 100 generations of 1,000 nodes, bounding current-generation reads. Valid
Append/Replace commits separately exercise captured-head materialization and
acknowledging an older turn while a newer one remains pending. Expired claims and
stale receipts must fail rather than appear acknowledged. Synthetic plan bodies
are never decoded as application content.

`repeated_chat_observation_has_linear_history_work` complements those scoped and
catalog probes with 8 concurrent public `/chat/stream` Sessions over 16 rounds,
two real unscoped recovery owners, and the production HTTP observer talking to
an authenticated loopback Memoria stub. Capture starts before discovery and ends
after every observation has a durable receipt. All request, discovery, competing
claim, materialization, authority, checkpoint, fixture-heartbeat and verification
SQL remains in the raw total.
The workload creates its own database so unscoped owners cannot process another
test's Sessions in a shared catalog. It does not scope discovery to the test user.
The profile uses the supported static self-hosted
authority port; the existing scoped probe separately measures database-resolved
authority. It is not an MOI/UC network latency benchmark.

Each round asserts exactly 8 observation manifest rows and 16 outgoing messages;
read JSON bytes stay near the first-round amount and the HTTP payload size stays
constant with fixed-size synthetic facts. The stub also checks the current turn's
content, so repeatedly sending an old or empty payload cannot satisfy the budget.
`CanonicalObservationRead` counts serialized manifest/content JSON returned by
storage, excluding claim/checkpoint metadata, fork-lineage metadata, physical disk
or MySQL protocol bytes. Prepared plans separately
check actual storage scan input for the same-generation suffix predicate. The
report includes complete raw SQL and SQLx returned rows, separately identified verification/control/
fixture and exact SQLx pool-initialization queries, summed SQL time, response completion time and receipt completion
time. A separate frontend bound retains at most 17 core statements per warm
request in this static-authority profile, plus bounded exact authentication reads. Pool
initialization has its own connection-count bound and remains in raw totals. The
four claim/checkpoint statements, one content read, discovery and competing
claims are disclosed separately and remain in the complete raw ceiling.
Its raw ceiling is deliberately above the former volatile 20-statement
budget: this test bounds and discloses coordination cost, not a claim that durable
recovery reduces statement counts. No captured SQL is discarded.

Run the exact ignored test in its own process. The services regression also
checks 20 valid turns, coalesced unacknowledged suffixes, concurrent completions,
replacement, divergent roots/totals/owners and stale/expired acknowledgements.
The HMAC fork journey checks the first inherited snapshot, incremental child
observation, and recovery after ancestor deletion and collection.

Run it with `ASTRA_TEST_DB_IT=1 cargo test -p astra-services --lib
session_observation::tests::retained_catalog_plans_keep_payload_reads_selective
-- --exact --ignored --nocapture` against a dedicated local database.

`observer_expiry_during_dispatch_does_not_acknowledge_progress` expires a claim
while the backend is held. `denied_recovery_never_reads_manifest_payloads` captures
SQL and history counters, requiring denial before manifest loading or payload
serialization. Run each exact ignored test in its own process, including the
managed-loop test, because SQL tracing uses a global subscriber.

The offline `ephemeral_burst_does_not_hold_cleanup_and_cancellation_releases_capacity`
and `ephemeral_observer_burst_delivers_terminal_and_releases_run_resources` tests
hold optional observers while verifying process capacity, explicit overload,
terminal delivery, run-resource cleanup and shutdown cancellation. They use no
database or live model provider.

The readonly-round comparison accounts for exact periodic control reads and
at most one account/session authentication snapshot when its 30-second cache
expires. Those statements still count toward the measured raw total and its
time-bounded envelope; repeated authentication snapshots or any other extra
readonly-round SQL fail.
The HTTP fixture binds both the Memoria URL and its master credential to the
strict local mock. Enabling self-hosted access exercises the authenticated
observer request, including its authority queries; a run with access disabled
does not validate that HTTP path. Wrong credentials still fail authentication.

### Durable provider concurrency and cancellation

The ignored runtime test
`db_multi_user_sessions_keep_provider_capacity_isolated_and_reusable` runs the
durable lifecycle against a loopback HTTP/SSE model gateway. It keeps one
provider request open, verifies that another writer for the same Session is
rejected, proves a different user cannot read, attach, or cancel that run, and
requires independent Sessions for two users to complete while the first run is
still active. It then verifies a terminal reader replay, durable cancellation,
reservation release, and reuse of the cancelled Session plus a new Session.

Run this focused check only with a disposable MatrixOne database and an
admission snapshot of at least three global provider slots and two slots per
owner:

```bash
ASTRA_TEST_DB_IT=1 cargo test -p astra-runtime --lib \
  db_multi_user_sessions_keep_provider_capacity_isolated_and_reusable -- \
  --ignored --nocapture
```

This is an execution isolation and lifecycle contract. Its bounded timeouts do
not claim deployment-scale throughput; use the Work pressure and multi-server
capacity probes for many readers, multiple server processes, provider quotas,
and latency measurements.

### Sustained ingestion and shared-pool pressure

The collision-receipt test separates the normal database contract from scale
diagnostics. Its normal workload processes 1,024 distinct conflicting payloads
through the bounded batch writer, plus concurrent single-receipt checks. To run
1,024 independent transactions contending on one identity, opt in explicitly:

```bash
# Use a dedicated test database and credentials from your environment/.env.
ASTRA_TEST_DB_IT=1 ASTRA_TEST_STORAGE_SCALE=1 \
cargo nextest run -p astra-services --test observation_capture_db_it \
  --run-ignored all --profile strict-online-ci --test-threads 1 \
  --success-output immediate \
  -E 'test(collision_receipts_bound_distinct_hashes_and_isolate_owners)'
```

The scale mode retains the same 60-second online hard deadline and fails on timeout;
it is not enabled by ordinary integration CI. Keep source, database, machine,
test profile and workload identical for before/after comparisons. A same-row
contention result is not a multi-session capacity result; batching does not
remove contention between independent transactions on the same identity.

For a short batching tradeoff comparison, run the ignored
`ingestion_batch_tradeoff_db_it` test against a dedicated disposable database.
Enable `--features capacity-probes` explicitly; it is not part of the ordinary
live integration lane.
It uses the default batch size and flush interval, an eight-connection test
pool, and 1,000 events with 256-byte content spread across either 1,000 or 10 Sessions,
with three repeats. It reports first/all database visibility, resolved flushes,
and concurrent `SELECT 1` latency with sample counts. Use the identical test
source on both revisions and a fresh database per revision; execute sequentially
without competing builds or load. Different schema/capture implementations make
this a full-revision comparison, not a pure COMMIT-cost measurement. Initial
session fence creation is included; fixture seeding and cleanup are not timed.
The finite test-profile workload is not a sustained-capacity benchmark, and
foreground percentiles with few samples must not be treated as an SLO.

```bash
ASTRA_TEST_DB_IT=1 ASTRA_DATABASE=astra_test_probe_batch \
cargo test -p astra-services --features capacity-probes \
  --test ingestion_batch_tradeoff_db_it -- --ignored --nocapture
```

For a short correctness check, the ignored live test
`shared_limiter_workers_recover_from_fences_without_blocking_foreground` runs
two ingestion workers with one shared SQL pool and one two-attempt limiter.
It verifies that held Session fences time out without starving the other
worker or unrelated foreground reads/writes, that connections are recovered,
and that releasing the fences drains each delivery exactly once. Run it with
`ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test event_ingestion_db_it
shared_limiter_workers_recover_from_fences_without_blocking_foreground -- --ignored --exact`.
This is same-process fault isolation, not cluster-wide admission or throughput.

The ignored `ingestion_process::cross_process_identity_and_delete_fence` test
in the same binary starts two real child processes with independent SQL pools.
It holds a durable Session fence while an unrelated Session progresses, then
requires an identical event submitted by both processes to produce one insert
and one replay with one session increment and one parent edge. A subsequent
write after canonical Session deletion must be rejected without resurrection.
The parent uses bounded IPC waits and kills/reaps its children on assertion
failure. This proves durable cross-process correctness, not cluster throughput.
Select this parent test explicitly; `ingestion_process::child` is only its
internal subprocess entrypoint. Credentials remain in environment/local `.env`.

The optional ingestion probe exercises the production ingestion queue and a
shared SQL pool with 100 synthetic owners and 1,000 Sessions. It is deliberately
separate from ordinary integration CI. Use Python 3.11 or newer, the pinned Rust
toolchain, and a dedicated disposable `astra_test_probe_*` database. Configure
database access through the existing environment or local `.env`; never put
credentials in scripts, command arguments, or published evidence.

```bash
# Offline harness and report checks; no database required.
cargo test -p astra-services --features capacity-probes --test ingestion_capacity_db_it
python3 -m unittest discover -s scripts/load -p test_ingestion_capacity_report.py

# Real database: 60-second warmup, then 600 seconds of measured arrivals.
python3 scripts/load/ingestion_capacity_probe.py \
  --database astra_test_probe_capacity --rate 500 --distribution hot

# Use the exact evidence path printed by the preceding command.
python3 scripts/load/ingestion_capacity_report.py \
  target/expriment/ingestion-capacity/REPLACE_WITH_RESULT.json
```

`uniform` spreads arrivals over all Sessions; `hot` sends half to one owner's
ten Sessions. Events mix 20% critical and 80% telemetry priority, with complete
serialized envelopes of 1 KiB, 16 KiB, and 128 KiB, targeting a seeded
80%/15%/5% distribution. Independent
foreground reads and writes share the pool, with a baseline measured before
ingestion starts. Default pool size is 32 and foreground rate is 20 operations
per second. These are declared probe parameters, not product-wide SLOs.

The generator uses absolute arrival deadlines and reports missed arrivals; it
does not hide overload by slowing the declared arrival rate. Per-delivery
observations distinguish commit, replay, rejection, and uncertain outcomes,
including cancellation during pool or transaction waits. Reports reconcile
durable identities, payload hashes, per-session counts, and parent edges.
Observer capacity is bounded; unavailable observation invalidates measurement
completeness without suppressing offered work.

Evidence is written under ignored `target/expriment/ingestion-capacity/`. It
includes source and binary fingerprints, compiler profile, parameters, bounded
timing histograms, backlog samples, and structured reconciliation results—not
raw database logs or credentials. Leave compiled sources unchanged and avoid
competing builds or probes during a run. The current launcher uses the Cargo
test profile; inspect the recorded compiler profile before comparing results
with optimized production builds. Percentiles are quantized upper bounds, not
exact latency measurements.

The report exits nonzero for malformed or ineligible evidence. It requires at
least 60 seconds of warmup and 600 measured seconds, arrival misses no greater
than 0.1%, complete accounting, no rejection or unresolved/late outcome, progress
for every owner, bounded backlog and age, and preserved foreground latency.
Sampling must cover the measured window with gaps no greater than 2.5 seconds.
A short smoke run cannot establish sustained capacity. Repeat the highest
passing rate and test both distributions before publishing a capacity claim.
Single-process ingestion evidence does **not** establish concurrent agent-turn
capacity, multi-server fairness, or fault recovery; those require separate
scenarios. Completed runs clean up only their uniquely prefixed fixture owners;
interrupted runs may leave fixture data in the designated disposable database.

## Recommended Workflow

### Optional thinking-protocol compatibility checks

Offline checks require no credentials:

```bash
CARGO_INCREMENTAL=0 cargo test -p astra-core --lib model_wire::
CARGO_INCREMENTAL=0 cargo test -p astra-services --lib models::
CARGO_INCREMENTAL=0 cargo test -p astra-runtime --lib turn::llm::
```

Explicit real-provider checks accept a private configuration file whose last
three nonempty lines are endpoint URL, API key, and upstream model ID. Do not
commit that file or print its contents. Each check makes paid provider calls:

```bash
ASTRA_TEST_SUMMARY_CONFIG_FILE=/absolute/path/to/private-config \
ASTRA_TEST_SUMMARY_MODEL=kimi-k2.6 \
CARGO_INCREMENTAL=0 cargo test -p astra-services --features live-provider-tests --lib \
  live_thinking_protocol_probe -- --ignored --nocapture

ASTRA_TEST_SUMMARY_CONFIG_FILE=/absolute/path/to/private-config \
ASTRA_TEST_SUMMARY_MODEL=kimi-k2.6 \
CARGO_INCREMENTAL=0 cargo test -p astra-runtime --features live-provider-tests --lib \
  live_work_admission_provider_contract -- --ignored --nocapture
```

The real-provider token-usage harness uses the same explicit build boundary:
`make harness-live-llm` enables `live-provider-tests` for
`live_token_usage_e2e`. Normal offline/online builds do not include this harness
code. Provider credentials or inherited environment settings cannot enable
paid calls in those lanes. Ordinary integration coverage uses controlled mock
providers.

Both paid checks require `live-provider-tests` and `--ignored`; default
MatrixOne CI can run ignored tests without a paid key. Do not enable this
feature in the generic online lane.

Repeat for `kimi-k3`. The first check verifies observable enabled/disabled
behavior; the second uses the actual streaming summary transport and Work
decision parser with a test persistence implementation, without starting an
Astra Server. Its elapsed time is invocation duration, not user-visible TTFT.
Neither replaces the MatrixOne-backed persistence fixture above. Configure the
Server's system DNS if needed; do not disable the BYOK public-endpoint policy
to run these checks.

```bash
# 1. Smallest relevant target while iterating
cargo test --manifest-path Cargo.toml -p astra-runtime --test http_contract

# 2. Core HTTP contract smoke
make test-contract

# 3. Full workspace + server E2E hooks (no online #[ignore] suites)
make test-offline

# 4. With MatrixOne + Memoria up: add online #[ignore] suites (same as the second half of `make test`)
make test-online
```

## Durable session-memory regressions

Use a designated isolated MatrixOne database and the environment described in
this guide; these tests must not run against QA, production or a populated local
database. The runtime fixture uses a loopback Memoria backend and synthetic
identities, with no live model or Memoria credentials.

```bash
ASTRA_TEST_DB_IT=1 ASTRA_TEST_DATABASE="$ASTRA_DATABASE" \
  cargo test -p astra-services --test session_memory_lifecycle_db_it -- --ignored --test-threads=1
ASTRA_TEST_DB_IT=1 ASTRA_TEST_DATABASE="$ASTRA_DATABASE" \
  cargo test -p astra-runtime --lib session_memory::durable::tests -- --ignored --test-threads=1
cargo test -p astra-cli --lib cli::session::session_startup::tests::cli_ -- --test-threads=1
```

The database tests cover canonical-context handoff recovery, shared fencing and
lease takeover. Runtime tests exercise authenticated exit/close/status HTTP
routes and partial-stage retry after lost receipts. CLI tests cover distinct
Astra/Memoria owners and unsuccessful queue acknowledgments without purge.

## Real Memoria observer authentication smoke

`memoria_observer_live` checks the production observer worker against a real
Memoria with owner-scoped authentication support (#250 or later) and its database.
Use only a dedicated local test service. Supply `ASTRA_TEST_MEMORIA_OBSERVER_URL`
(a loopback HTTP origin) and `ASTRA_TEST_MEMORIA_OBSERVER_KEY` privately through
the test process environment; do not put credentials in command arguments or
commit them. The test creates unique synthetic owners and retains their test
memories. It rejects remote origins and redirects.

```bash
ASTRA_TEST_MEMORIA_OBSERVER_LIVE=1 cargo test -p astra-runtime \
  --features live-provider-tests --test memoria_observer_live \
  live_memoria_owner_observe -- --ignored --exact --nocapture
```

The feature, ignored-test selection and explicit opt-in are all required, so
ordinary offline and database CI lanes cannot enable it via inherited credentials.
It verifies that the legacy impersonation header fails with 400, the production
self-hosted observer succeeds with `X-User-Id`, the target owner's session contains
stored memories, and a second owner's view of the same session is empty. This is
an observer authentication/storage check, not MOI identity or prompt recall E2E.
The deterministic `turn_observer_authority` target remains the offline contract
suite; it does not substitute for the real-backend smoke.

## What "done" Means

A change is not complete until:

- formatting passes
- compile/type checks pass
- clippy passes
- the relevant Rust tests pass (including PR Matrix E2E when touching server/persistence paths)
