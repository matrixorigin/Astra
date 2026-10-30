# Test coverage matrix (capabilities vs tests)

This document maps **user-visible capabilities** to **where they are tested**, so stub/JSON contract tests can be removed without losing the last line of defense. It extends [`system-e2e-matrix.md`](./system-e2e-matrix.md).

Legend: **E2E** = `crates/runtime/tests/system_matrix_http_e2e/` with `ASTRA_TEST_DB_IT=1` (real MatrixOne + HTTP + `sqlx` where noted). **Stub** = `build_app(AppState::new(...))` with in-memory/stub services (no DB). **Unit** = `#[cfg(test)]` in crate sources or small integration tests without HTTP.

## Configuration (`AppSettings`)

| Capability | E2E | Removed stub | Other |
|------------|-----|--------------|-------|
| Env map defaults / overrides / embedding model errors vs `fixtures/contracts/settings_contract.json` | — | `config_contract` | `astra-core` `config::settings_contract_tests` |

## Core API (auth, sessions, agents, events)

| Capability | E2E (`system_matrix_http_e2e`) | Removed stub tests (replaced by E2E) | Other |
|------------|-------------------------------|--------------------------------------|-------|
| Register / login / refresh / me / logout | `harness::bootstrap`, `journey_full` | `auth_contract` | — |
| `GET /`, `GET /health` (metadata + DB + persist counters) | `e2e_matrix_meta_health` | — | — |
| Auth errors (no session token, duplicate user, bad password) | `journey_extended::run_auth_and_session_negative_paths` | `auth_contract` (stub errors) | — |
| Sessions list/get/put, close/resume, activity | `journey_full` + DB `agent_sessions`; focused `GET`/`PUT` vs row in `e2e_matrix_session_http_db` | `session_contract` | — |
| Session cancel + delete + 404 | `journey_extended` + DB | `session_contract` | — |
| Agents CRUD + DB | `journey_full` + `agent_agents` | `agent_crud_contract` | — |
| Events create/get/list/causal chain | `journey_full` | `event_crud_contract` | — |

## Product API (context, decisions, memory proxy, jobs, sandbox, …)

| Capability | E2E | Removed stub tests |
|------------|-----|-------------------|
| Context + decisions + DB | `journey_full` + `e2e_matrix_context_decision_chain` (dedicated event→context→decision + `ctx_*` SQL) | `context_contract`, `decisions_contract` |
| Memory proxy isolation (`user_id` / `session_id` overwrite) | `journey_extended::run_memory_proxy_user_isolation` | `memory_contract` |
| Skills + introspection reads | `journey_full` | `skills_contract`, `introspection_contract` |
| Jobs + webhook | `journey_full` | `jobs_contract` |
| Workflows list | `journey_full` | `workflows_contract` |
| Evaluation read paths (x-user-id, trust/SLO/observability need agent) | `e2e_matrix_evaluation_reads` | — |
| Sandbox CRUD + DB | `journey_full` | `sandbox_contract` |
| Triggers + fire + delete + DB | `journey_full` | `triggers_contract` |
| Marketplace probe | `journey_full` | `marketplace_contract` |
| Data versioning lineage | `journey_full` | `data_versioning_contract` |
| `GET /models` (authenticated paginated catalog) | `journey_full` + `e2e_matrix_models` | cursor continuation, global total, revision stability |
| Models admin CRUD + `infra_llm_models` | `journey_extended::run_models_admin_crud_with_db` (`provider: mock`, `grant_astra_admin_role`) | `model_crud_contract` |
| `GET /admin/tokens` (403 → grant `astra_admin` → 200 array) | `e2e_matrix_saas_admin_tokens_rbac_smoke` | — |
| Delegation `GET .../delegations` + `POST .../delegate` validation failure (`400`) | `e2e_matrix_delegate_http_boundaries` | — |
| Reflect + decision-trace (authenticated) | `journey_full` (`GET .../reflect`, `GET .../decision-trace`) | `reflect_contract` (stub) |
| Skill config CRUD (in-memory stub) | — (future E2E or `astra-runtime` unit tests) | `skill_config_contract` |
| Memory branches API (stub `X-User-Id` auth) | — (future E2E with JWT) | `branches_contract` |
| `POST /streaming/chat` (stub `X-User-Id`) | — (prod uses configured `StreamingService`; `journey_extended` covers `POST /chat/stream` SSE) | `streaming_contract` |
| Route registration (no accidental 404 on major paths) | `runtime/src/server/router_builder.rs` `#[cfg(test)]` (`route_count_regression`, `critical_route_paths_exist`, `all_api_groups_have_routes`) + Matrix `journey_full` | `route_registry_contract` (HTTP smoke) |
| `SharedPool` on `AppState` / auth+session service constructors | Compile-time API + real pool in services tests / Matrix E2E | `shared_pool_contract`, `shared_pool_migration_contract` |
| Thinking models: `reasoning_content` on every assistant+`tool_calls` after mid-session switch / DB recovery | `runtime/src/turn/edge_ledger.rs` `#[cfg(test)]` (`mid_session_switch_*`, `append_recovered_events` + `strip_stale_reasoning` pipeline) | — (avoid extra integration binary) |

## ChatGPT subscription connections

Offline coverage lives in `astra-core::model_wire::responses` (wire codec),
`astra-services::chatgpt::tests` (OIDC, redirect and feature gates),
`astra-services::auth::encryption::tests` (keyed-digest verification),
`astra-cli::cli::model_connections::tests` (callback/browser binding), and
`runtime/tests/user_models_http.rs` (authenticated routes and frozen catalog).
`services/tests/chatgpt_connections_db_it.rs` uses an explicitly isolated
MatrixOne database and a local fake OAuth/Responses provider for encryption,
ownership, rotation races, model sharing, disconnect and reauthorization.
The reauthorization/disconnect race observes the blocked completion SQL before
cancelling its attempt, and verifies cancellation without a second code exchange.
The identity journey reuses one SQL connection from initial registration through
reauthorization and verifies its persisted connection/subject fences.
Refresh regressions cover a refused TCP connection followed by recovery,
cross-instance expiry with 11-second discovery/JWKS/token legs, and metadata
cache reuse, rejected refresh rate limits and probe 401 recovery. Cancellation of the last refresh waiter must still commit the new
credential; 64 concurrent waiters share one rotation. Row-lock races exercise
model creation, checking, activation and default selection against disconnect.
The ignored `responses_transport_recovery_and_credential_fencing`
transport test covers physical-attempt settlement, manually constructed tool
frames, a delayed old-token 401, context-window and in-stream quota classifications,
failed-response usage preservation, and disconnect during
headers. Sixteen concurrent requests verify that SQL fencing is shared per
connection. A refused-connection retry with a 100 ms invocation budget must
expire during its one-second backoff before admitting a second attempt.
Transient HTTP admission failures exercise the shared retry runner and preserve
the structured subscription error after physical retries are exhausted.
Paired OpenAI/Responses projection coverage checks runtime fact/instruction roles,
tool/result order, prefix stability, and private continuation. The transport
journey supplies raw canonical messages to exercise direct request preparation.
The ignored `chatgpt_subscription_agent_uses_catalog_and_continues_persisted_session`
test in `runtime/tests/web_agent_e2e.rs` exercises authenticated connection and
model routes, real model-catalog tool execution through `/chat/stream`, and
canonical database history. It continues the same session with fresh Server
services and a new run engine, using an empty local journal directory, and
asserts provider tool results, encrypted reasoning and assistant phase survive.
This tests durable restoration within one test process; an operating-system
restart and live OpenAI consent/inference remain operator acceptance steps.
These database lanes do not contact OpenAI or spend subscription quota.

Run these DB lanes with the testing guide's isolated database variables:

```bash
cargo test -p astra-services --features e2e-hooks --test chatgpt_connections_db_it -- --ignored --test-threads=1
cargo test -p astra-runtime --lib responses_transport_recovery_and_credential_fencing -- --ignored --test-threads=1
cargo test -p astra-runtime --features e2e-hooks --test web_agent_e2e chatgpt_subscription_agent_uses_catalog_and_continues_persisted_session -- --ignored --test-threads=1
```

Live acceptance remains an explicit operator step on an isolated self-hosted
installation: enable the feature/host ID, run the documented CLI linking flow,
authorize plan use, configure two visible models sharing one connection, run a
text/tool turn, close the CLI and resume the former session, then disconnect
and verify dependent models fail closed. Reconnect and check a model to recover.
This requires real browser consent and consumes plan usage; do not make it a
required fork CI lane or reuse a deployment credential cache. Record the exact
OpenAI model catalog and source SHA without logging tokens or callback URLs.

## Large integration binaries (audit — not removed in this pass)

| Binary | Role | E2E overlap | Recommendation |
|--------|------|-------------|----------------|
| `improvement_proofs.rs` | Token/budget/compaction **proofs** vs baselines | None (no HTTP/DB) | **Keep**; move overlapping cases into `astra-runtime` unit tests only if duplicates appear in `src/`. |
| `utterance_regression.rs` | Utterance/tool-selection regression | Partial overlap with `phase8_regression` / cloud routing | **Keep** for NLP surface; dedupe individual cases incrementally if two tests assert the same ranking. |
| `web_agent_e2e.rs` | Real `/chat/stream` journeys for CLI+Server, Server-only, and Edge+Server, including multi-round, cancellation, callback isolation, and errors | Supersedes the deleted client-owned bridge binaries | **Keep**; one production loop with deterministic provider fixtures. |
| Chat turn **pure helpers** (stall, state, persist, routing, cloud/history, …) | `#[cfg(test)]` next to each module under `crates/runtime/src/turn/` | Removed ~33 `chat_turn_*_contract.rs` + matching `fixtures/contracts/chat_turn_*.json` (duplicated JSON snapshots) |
| Run/chat lifecycle (stub `RunLifecycleService` + `/chat/stream` SSE) | — (Matrix journeys exercise `/runs` list and `journey_tasks_runs` for pause/resume) | `chat_lifecycle_contract` |
| Memory prefetch + mock Memoria HTTP | server loop/context unit tests | `memory_prefetch_contract` |
| Token / context budget / retrieval JSON tables | `crates/runtime/src/prompts/mod.rs`, `context.rs` `#[cfg(test)]` | `token_retrieval_contract` + `token_retrieval_contract.json` |

## Developer-loop coverage

- **Topology journeys:** `web_agent_e2e.rs` proves CLI+Server single admission, Server-only internal tools, Edge+Server callbacks, offline blocking, and unknown-run isolation.
- **Durability:** `system_matrix_http_e2e` covers concurrency, callbacks, and retention. `http_contract` rejects retired session re-execution routes; Thin Client and Run lifecycle tests cover recorded run event replay.
- **Logic:** prefer unit tests beside the canonical server loop and provider transports; do not introduce a second loop-shaped integration harness.

## Services crate

| Test | Gate |
|------|------|
| `astra-services` `multi_agent_integration` | `ASTRA_TEST_DB_IT=1` (PR CI when enabled in [`.github/workflows/test.yml`](../../.github/workflows/test.yml)) |

## Multi-agent coordination

| Capability | E2E (`system_matrix_http_e2e`) | Stub / integration | Other |
|------------|-------------------------------|----------------------|-------|
| Child pause, guidance, resume and final adoption | — | — | `astra-cli/tests/tui_pty_journey.rs::live_child_pause_guidance_resume_preserves_execution_identity` (opt-in real provider and isolated Server; see [testing guide](../guides/testing.md)) |
| Delegation mailbox and parent/child routing | — | — | `crates/runtime/src/messaging/orchestrator_mailbox_tests.rs` |

## How to run

- Offline slice: `make test-offline` (workspace + server E2E hooks; no online `#[ignore]` suites).
- Full validation with MatrixOne: `make test` (`test-offline` then `test-online`) or run `make test-online` alone when you only need ignored suites.
- Advanced: set `ASTRA_TEST_DB_IT=1` / `ASTRA_TEST_DB_IT=1` manually as in [`system-e2e-matrix.md`](./system-e2e-matrix.md).
