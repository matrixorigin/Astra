# Testing Guide

This repository is validated through Rust-first checks, contract tests (fast, many with stub services), and optional **live MatrixOne** system E2E.

## Primary Commands

```bash
# Full suite: workspace + e2e-hooks + online #[ignore] Matrix E2E + multi-agent integration (requires MatrixOne and Memoria for the online portion)
make test

# Workspace + server E2E hooks only (no online #[ignore] suites)
make test-offline

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

Direct `cargo` usage:

```bash
cargo test --manifest-path Cargo.toml -q
cargo check --manifest-path Cargo.toml
```

MCP CLI tests launch the prebuilt `mock_mcp_server` beside their test executable.
`make test-offline` builds this fixture before the workspace tests. For direct
`cargo test` or `cargo nextest` invocations that include MCP CLI tests, first run
`cargo build -p astra-cli --bin mock_mcp_server` using the same target directory,
target triple, and profile. Test processes do not run nested Cargo builds.

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
select the decoder. The harness requires one determined rubric category (fully
yes, substantially yes, partial, or no), then maps it to 1.0, 0.7, 0.4, or 0.0.
Judgment values are never scores; unknown, conflicting, and malformed rubric
judgments fail without format repair. Rubric wire IDs are descriptive strings
(`rubric_fully_yes`, `rubric_substantially_yes`, `rubric_partial`,
`rubric_no`), not numeric indices. Missing, extra, duplicate, and wrongly typed
answers remain invalid, not coerced. Criterion thresholds and quorum
aggregation remain unchanged. Each judgment and quorum vote creates its own
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

Live quality judging uses a bounded projection of the durable tool
journal. It preserves canonical run, turn, round, batch, and parallel metadata
when present; missing identity stays unknown and cannot prove batching. It
reserves room for call identities and statuses before sharing the
remaining budget across arguments and results, so a large early response does
not hide later verification. Truncated fields report their original and omitted
character counts; if even the call identities exceed the budget, the envelope
reports omitted calls. The projection fits the final judge prompt's existing
budget. Raw reports and deterministic checks retain the original evidence.
An inconclusive quality judgment caused by omitted evidence is not proof that
the agent's claimed result is false, and a passing exit code is not a substitute
for inspecting quality failures.

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
unexecuted initial item to be cancelled, requires fresh addition identities, and
checks that the remaining initial items and additions each have a delivered
settlement. Replays of the same execution do not count twice. Its optional
`cancellation_after_deliveries` minimum requires causal evidence that the target
remained unstarted until the preceding deliveries; a late snapshot alone is not
proof of deferred cancellation. Natural requests with unspecified cancellation
targets and requests for an explicit deferred replacement use separate cases.

## Where Tests Live

- `crates/runtime/tests/` — HTTP integration tests for `astra-runtime` (including `*_contract.rs`, `system_matrix_http_e2e/`, bridge E2E).
- `crates/services/tests/` — service-layer tests (e.g. `multi_agent_integration` with live DB when `ASTRA_TEST_DB_IT=1`).
- `fixtures/contracts/` — JSON fixtures for contract tests that load shared request/response shapes.
- Capability ↔ route ↔ E2E mapping: [`docs/testing/system-e2e-matrix.md`](../testing/system-e2e-matrix.md).
- SaaS capability test plan: [`docs/testing/saas-test-plan.md`](../testing/saas-test-plan.md) (`make test-saas`; Rust HTTP E2E plus optional remote `@astra/sdk` coverage).
- Coverage matrix (what replaced stub tests, large-binary audit): [`docs/testing/coverage-matrix.md`](../testing/coverage-matrix.md).

## Terminal resize and reflow

The inline TUI regression drives the real binary through a controlling PTY and
xterm.js's headless terminal (including native reflow). It checks repeated
narrow/wide and short/tall transitions, draft input, rapid resizes with delayed
cursor replies, one live footer/composer across the entire buffer, and
preservation of pre-existing and committed startup history. It uses a
synthetic token and an unavailable loopback endpoint; no model or real account
is needed. Python 3 and the repository's Node.js version are required.

```bash
cargo build --locked -p astra-cli --bin astra
npm ci --ignore-scripts --prefix scripts/tui-reflow
ASTRA_TEST_BINARY="$PWD/target/debug/astra" npm test --prefix scripts/tui-reflow
```

The `terminal-pty` CI lane runs it on Linux and macOS. The Rust terminal-reader
PTY tests also verify that a resize cursor query preserves keyboard/paste
input and times out when the terminal does not answer.

## Live MatrixOne system E2E

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
ASTRA_BYOK_DNS_SERVERS=tcp://223.5.5.5 \
ASTRA_TEST_BYOK_MODELS_URL=https://api.moonshot.cn/v1/models \
CARGO_INCREMENTAL=0 cargo test --locked -p astra-services \
  --features external-contract-tests --test byok_live_network -- --ignored
```

It expects HTTP 401 with no provider key and proves DNS/TLS/HTTP reachability,
not successful model inference.

The provider-wire regression uses a strict loopback HTTP fixture and a disposable
MatrixOne database, without real provider keys. Create the designated database
first and supply its `MATRIXONE_*` connection settings. Choose an unused loopback
port for the fixture:

```bash
ASTRA_TEST_DB_IT=1 ASTRA_DATABASE_PREFIX= \
ASTRA_DATABASE=astra_test_probe_local ASTRA_TEST_DATABASE=astra_test_probe_local \
ASTRA_ALLOW_INSECURE_DEFAULTS=1 \
ASTRA_BYOK_DEEPSEEK_BASE_URL=http://127.0.0.1:18994 \
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
  --ignored --nocapture
```

Requires the same environment as `astra-server`: `MATRIXONE_*`,
`ASTRA_JWT_SECRET`, `ASTRA_TOKEN_ENCRYPTION_KEY`, Memoria, and embedding
settings parsed by `astra_core::AppSettings::from_env`. Use a local `.env` if
you use one for development. To isolate from production on one MatrixOne host,
set **`ASTRA_DATABASE_PREFIX`** (effective DB = prefix + `ASTRA_DATABASE`).
Optionally set **`ASTRA_AUTO_CREATE_DATABASE=1`** so the first
`ensure_core_schema` (server or online tests) runs `CREATE DATABASE IF NOT
EXISTS` for that effective name (bootstrap catalog defaults to `mysql`).

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

The collision-receipt and canonical-WAL tests separate database contracts from
full-scale diagnostics. The normal collision contract still processes 1,024
distinct conflicting payloads through the bounded batch writer, plus concurrent
single-receipt checks. The normal WAL contract runs 32 rounds with the same
retry ownership, stale-parent rollback, linear payload, recovery, and retirement
assertions. To retain the original 1,024 **independent transactions contending
on one identity** and 300-round WAL workload, opt in explicitly:

```bash
# Use a dedicated test database and credentials from your environment/.env.
ASTRA_TEST_DB_IT=1 ASTRA_TEST_STORAGE_SCALE=1 \
cargo nextest run -p astra-services \
  --test observation_capture_db_it --test inference_execution_db_it \
  --run-ignored all --profile strict-online-ci --test-threads 1 \
  --success-output immediate \
  -E 'test(collision_receipts_bound_distinct_hashes_and_isolate_owners) | test(canonical_transition_wal_is_linear_and_recoverable_across_many_rounds)'
```

The scale mode retains the same 30-second hard deadline and fails on timeout;
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
Neither replaces the MatrixOne-backed persistence fixture above. Configure
`ASTRA_BYOK_DNS_SERVERS` only when required by the local network; do not disable
the BYOK public-endpoint policy to run these checks.

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

## What "done" Means

A change is not complete until:

- formatting passes
- compile/type checks pass
- clippy passes
- the relevant Rust tests pass (including PR Matrix E2E when touching server/persistence paths)
