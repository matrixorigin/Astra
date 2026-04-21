# System E2E matrix (HTTP + MatrixOne)

This document maps **user-visible capabilities** to **HTTP routes**, **persistence (MatrixOne tables or in-process stores)**, and the **integration tests** that assert them. It complements `router_builder.rs` unit tests (route registration only).

**Layout (system E2E plan):** the integration binary is `rust/crates/runtime/tests/system_matrix_http_e2e/main.rs` with shared **`harness.rs`** (env gate, HTTP, `sqlx`, `bootstrap`) and **`journey_full.rs` / `journey_tasks_runs.rs` / `journey_extended.rs`**. `Cargo.toml` names the test target `system_matrix_http_e2e` and enables `bridge-e2e-hooks`.

## How to run

```bash
cd rust
ASTRA_SYSTEM_MATRIX_E2E=1 \
ASTRA_BRIDGE_TEST_SECRET=system-matrix-e2e-secret \
cargo test -p astra-runtime --test system_matrix_http_e2e --features bridge-e2e-hooks -- \
  --ignored --nocapture
```

Requires the same environment as `astra-server`: `MATRIXONE_*`, `JWT_SECRET_KEY` / `SECRET_KEY` and related keys via `astra_core::AppSettings::from_env`, etc. Use a local `.env` if you use one for development.

## Environment variables (对照表)

| Variable | Role | Notes |
|----------|------|--------|
| `ASTRA_SYSTEM_MATRIX_E2E` | **Gate** | Must be `1` or ignored tests panic in `require_system_e2e_env` |
| `ASTRA_BRIDGE_TEST_SECRET` | `/chat/turn` E2E | Injected before parallel runs; must match bridge hook expectations in full journey |
| `MATRIXONE_HOST` | DB | Default `localhost` |
| `MATRIXONE_PORT` | DB | Default `6001` |
| `MATRIXONE_USER` | DB | Default `root` |
| `MATRIXONE_PASSWORD` | DB | Default dev password in `astra_core::runtime_limits` if unset |
| `ASTRA_DATABASE` | DB | Base name; default `astra_runtime` |
| `ASTRA_DATABASE_PREFIX` | DB | Optional; effective DB = prefix + `ASTRA_DATABASE` (e.g. `test_` + `astra_runtime`) |
| `JWT_SECRET_KEY` | Auth tokens | Default dev string if unset (not for production) |
| `SECRET_KEY` | App crypto | Default dev string if unset |
| `REDIS_HOST` / `REDIS_PORT` | Cache | Defaults `localhost` / `6379` |
| `EMBEDDING_*` | Embeddings config | `EMBEDDING_DIM` may be required for unknown models (see `AppSettings`) |
| `CHAT_TURN_BRIDGE_SECRET` | Bridge | Default dev string; align with deployment if using real bridge |
| `ASTRA_SYSTEM_MATRIX_E2E_TEST_THREADS` | Makefile only | Set to `1` to run `system_matrix_http_e2e` with `--test-threads=1` (serial) |

Evaluation **read** routes in the full journey use `x-user-id` without bearer (see `journey_full`). Other authenticated calls use the JWT from `bootstrap`.

## Test binaries (ignored by default)

Ignored tests in `system_matrix_http_e2e` avoid overlap with the full journey (e.g. no separate “basic session” test that repeats the same list/get/close/resume steps).

**Related (separate crate / gate):** `ASTRA_SERVICES_DB_IT=1` runs `cargo test -p astra-services --test services_db_integration -- --ignored` (MatrixOne): pagination clamps, `skills_registry` list/index, cross-session audit service paths, and `MatrixOneDurableTaskLifecycle::resume_task` verification history (see that test file’s module doc).

| Test name | File / module | Scope |
|-----------|---------------|-------|
| `product_matrix_api_journey_hits_multiple_tables` | `journey_full.rs` | Full journey: sessions (list/get/put, close/resume, activity, **platform snapshot**), agents, events, context, decisions, memory proxy, edge, jobs, sandbox, triggers, skills, introspection, learning (signals/stats + **POST /api/v1/learning/feedback** after `chat/turn`), evaluation reads, marketplace probe, `chat/turn` SSE + `agent_events`, audit/replay, logout |
| `e2e_matrix_tasks_lease_and_db_assertions` | `journey_tasks_runs.rs` | `POST /tasks`, `GET /tasks`, `GET /tasks/{id}`, `GET .../progress`, `agent_tasks`; edge register; lease claim / `GET` lease / renew / release; `task_leases`; `PUT /tasks/{id}/status` |
| `e2e_matrix_chat_run_pause_resume_http` | `journey_tasks_runs.rs` | `POST /chat` (background run), `POST .../pause`, `GET /chat/runs/{id}`, `POST .../resume` |
| `e2e_matrix_session_cancel_delete` | `journey_extended.rs` | `POST /sessions/{id}/cancel` + `agent_sessions.status`, `DELETE /sessions/{id}` |
| `e2e_matrix_chat_stream_session_info` | `journey_extended.rs` | `POST /chat/stream` SSE → `session_info` + `run_id` |
| `e2e_matrix_approval_respond_invalid_session_id` | `journey_extended.rs` | `POST /approval/respond` with an unsafe `session_id`; assert `400` rejects invalid journal path components |
| `e2e_matrix_edge_callback_http_boundary_failures` | `journey_extended.rs` | `POST /tools/result` and `/approval/respond` without auth or with malformed payloads; assert auth/client errors at the HTTP boundary |
| `e2e_matrix_duplicate_tool_result_idempotency` | `journey_extended.rs` | Start `POST /chat/turn`, then `POST /tools/result` twice for the same `request_id`; assert the initial SSE handoff still emits one `tool_request` and ends with `has_tool_calls=true` |
| `e2e_matrix_duplicate_approval_response_idempotency` | `journey_extended.rs` | `POST /approval/respond` twice for the same `request_id` + `session_id`; assert one persisted `approval_decision` in the session journal |
| `e2e_matrix_chat_turn_partial_batch_failure` | `journey_extended.rs` | Start one `POST /chat/turn` round that emits two `tool_request`s; reply with one success and one failure, then assert the initial SSE handoff still ends with `has_tool_calls=true` |
| `e2e_matrix_chat_turn_out_of_order_tool_results` | `journey_extended.rs` | Start one `POST /chat/turn` round that emits two `tool_request`s; send the second callback result before the first and assert the initial SSE handoff still ends with `has_tool_calls=true` |
| `e2e_matrix_same_session_concurrent_turns_isolated` | `journey_extended.rs` | Launch two concurrent `POST /chat/turn` requests against the same session; assert both SSE responses complete and persisted `llm_response` rows keep distinct `event_id` / `causal_chain_id` values |
| `e2e_matrix_same_session_waiting_turn_overlap_isolated` | `journey_extended.rs` | Start one same-session tool-backed `POST /chat/turn`, finish a second same-session plain turn while the first is still in handoff, then assert the two SSE streams do not leak each other’s content |
| `e2e_matrix_auth_session_negative_paths` | `journey_extended.rs` | `GET /sessions` without auth (401); mode-aware auth negatives: `local_jwt` checks duplicate register + bad login + successful login, `trusted_moi` checks local auth endpoints disabled |
| `e2e_matrix_memory_proxy_user_isolation` | `journey_extended.rs` | Unauthenticated `POST /memory/store` (401); spoofed `user_id`/`session_id` in body → forwarder receives JWT `user_id` for both fields |
| `e2e_matrix_models_admin_crud` | `journey_extended.rs` | Mode-aware: `local_jwt` runs SQL `astra_admin` role grant + `POST/PUT/DELETE /models` with DB checks; `trusted_moi` asserts current admin path rejects the call (admin auth still local-JWT based) |
| `e2e_matrix_audit_cross_session_analytics_http` | `journey_audit_cross_session.rs` | Seed `agent_sessions` / `agent_events` / `ctx_decision_audits`; `GET /audit/stats`, `GET /audit/mutations`, `GET /audit/promotions` JSON assertions |
| `e2e_matrix_trusted_moi_user_system_integration` | `journey_trusted_moi.rs` | Startup in `trusted_moi`; external JWT `/auth/me`; local auth endpoints disabled (`/auth/register|login|refresh`=403); `POST /sessions` + DB owner and memory proxy identity isolation bound to upstream user ID |

Shared helpers: `tests/system_matrix_http_e2e/harness.rs` (`bootstrap`, `bootstrap_trusted_moi`, `grant_astra_admin_role`, HTTP helpers, `cleanup_*`, row getters, SSE helpers, `wait_for_agent_event_types` — polls `agent_events` after `chat/turn` instead of a fixed sleep).

## Database isolation

- **Shared database**: All tests use the same MatrixOne database from `AppSettings` (typically `astra_runtime`). There is **no separate schema per test**.
- **Row isolation**: Each `bootstrap()` registers a **new user** (`prod_matrix_{uuid}`), and `bootstrap_trusted_moi()` creates a **new external user principal** (`moi-user-{uuid}`); both create a **new** `session_id` and use an `edge_agent_id` / `suffix` unique to that run. API state and SQL assertions are scoped by those IDs.
- **Parallel runs**: Tests are safe to run in parallel by default (`cargo` / `make test-online` without `ASTRA_SYSTEM_MATRIX_E2E_TEST_THREADS=1`). The full journey uses a **suffix-scoped marketplace skill name** (`e2e_matrix_mkt_{suffix}`) so concurrent runs do not fight over the same global marketplace stats key.
- **Opt-in serial**: If you hit flakiness (shared Redis keys, connection limits, etc.), run with `ASTRA_SYSTEM_MATRIX_E2E_TEST_THREADS=1` (see `Makefile` `test-ignored-integration`) to force `--test-threads=1` for `system_matrix_http_e2e` only.

## API groups vs coverage (P0 / P1)

Legend: **DB** = SQL assertion on MatrixOne; **HTTP** = response-only; **—** = not covered by system E2E yet.

| Group | P | Representative routes | Persistence check | Test(s) |
|-------|---|----------------------|-------------------|---------|
| Meta | P0 | `GET /health`, `GET /` | — | `product_matrix_*` |
| Auth | P0 | `/auth/register`, `/login`, `/refresh`, `/me`, `/logout` | `auth_users` | Every test uses `bootstrap` (register/login); `product_matrix_*` also hits `/auth/refresh` and `/logout` |
| Sessions | P0 | `/sessions`, `.../close`, `.../resume`, `.../cancel`, `DELETE ...`, `.../activity` | `agent_sessions` | `product_matrix_*` + `e2e_matrix_session_cancel_delete` |
| Session audit | P0 | `/sessions/{id}/audit/*`, `/audit/*` | mostly HTTP | `product_matrix_*`; `e2e_matrix_audit_cross_session_analytics_http` (`/audit/stats`, `/audit/mutations`, `/audit/promotions` + DB seed) |
| Agents | P0 | `/agents` CRUD | `agent_agents` | `product_matrix_*` |
| Models | P1 | `GET /models`, admin `POST/PUT/DELETE /models` | `infra_llm_models` | `product_matrix_*` (list); `e2e_matrix_models_admin_crud` (admin CRUD + DB) |
| Events | P0 | `/events`, causal chain, session events | `agent_events` | `product_matrix_*` |
| Context | P0 | `/context` | `ctx_snapshots` | `product_matrix_*` |
| Decisions | P0 | `/decisions`, audit | `ctx_decision_audits` | `product_matrix_*` |
| Memory proxy | P1 | `/memory/*` | Memoria stub calls | `product_matrix_*` |
| Edge §5.5 | P0 | `/agents/edge`, `/tools/result`, `/approval/respond` | `edge_agent_registry` | `product_matrix_*`, tasks lease (edge register), `e2e_matrix_approval_respond_invalid_session_id`, `e2e_matrix_edge_callback_http_boundary_failures`, `e2e_matrix_duplicate_tool_result_idempotency`, `e2e_matrix_duplicate_approval_response_idempotency` |
| Jobs | P1 | `/jobs`, `/jobs/webhook` | service persistence | `product_matrix_*` |
| Sandbox | P1 | `/sandbox` | `infra_sandbox_metadata` | `product_matrix_*` |
| Triggers | P1 | `/triggers`, fire, delete | `wf_triggers` | `product_matrix_*` |
| Skills / introspection | P1 | `/skills`, `/introspection/*` | mixed | `product_matrix_*` |
| Learning | P1 | `/api/v1/learning/*` | — | `product_matrix_*` |
| Evaluation | P1 | `/evaluation/*` reads | — | `product_matrix_*` |
| Evaluation (writes) | P1 | `POST` gate/validate, drift/run, loop | — | — (no system E2E; add when implementations return success) |
| Marketplace | P1 | quality report, stats, search | marketplace stats tables | `product_matrix_*` |
| Chat turn (SSE) | P0 | `POST /chat/turn` + bridge secret | `agent_events` | `product_matrix_*`, `e2e_matrix_edge_callback_http_boundary_failures`, `e2e_matrix_duplicate_tool_result_idempotency`, `e2e_matrix_duplicate_approval_response_idempotency`, `e2e_matrix_chat_turn_partial_batch_failure`, `e2e_matrix_chat_turn_out_of_order_tool_results`, `e2e_matrix_same_session_concurrent_turns_isolated`, `e2e_matrix_same_session_waiting_turn_overlap_isolated` |
| Chat / runs | P0 | `POST /chat`, `/chat/stream`, `/chat/runs/*` | **In-memory** run store in `build_server_state` (not Matrix table today) | `e2e_matrix_chat_run_pause_resume_http`, `e2e_matrix_chat_stream_session_info` |
| Tasks | P0 | `/tasks`, `GET` list/get/progress, `/tasks/{id}/lease/*`, `.../status` | `agent_tasks`, `task_leases` | `e2e_matrix_tasks_lease_and_db_assertions` |
| Platform | P1 | `GET /platform/snapshot` | — | `product_matrix_*` |
| Workflows | P1 | `GET /workflows` | — | `product_matrix_*` |
| Data versioning | P1 | lineage GETs | — | `product_matrix_*` |
| Replay | P1 | `/sessions/{id}/replay/compare` | — | `product_matrix_*` |
| Branches | — | `/branches/*` | — | — |
| Admin | — | `/admin/*` | — | — |
| WebSocket | — | `/chat/ws` | — | — |
| Delegation | — | `/chat/runs/.../delegate` | — | — |
| Team | — | `/teams`, `/teams/{name}`, `/teams/{name}/execute`, `/teams/{name}/executions`, `/teams/.../snapshots` | `team_definitions` / related tables when using DB-backed store | Offline HTTP: `team_api_integration` + **`team_execute_http_integration`** (mock delegation); no `system_matrix_http_e2e` journey yet — see [`coverage-matrix.md`](./coverage-matrix.md) **Team** |

## CI

- **PR** (`.github/workflows/test.yml`): MatrixOne + Redis service containers; single `make test` step with `ASTRA_SYSTEM_MATRIX_E2E`, `ASTRA_SYSTEM_MATRIX_E2E_TEST_THREADS`, and `ASTRA_MULTI_AGENT_IT` set for ignored **`system_matrix_http_e2e`** and **`multi_agent_integration`**. See also [`coverage-matrix.md`](./coverage-matrix.md).
- **Manual / nightly**: `.github/workflows/e2e-matrix-nightly.yml` — `workflow_dispatch` with optional **test name filter** (substring) to run a subset (e.g. `e2e_matrix_tasks`) or leave empty for all ignored tests in the binary.

## Router groups alignment

Same prefixes as [`router_builder` `all_api_groups_have_routes`](../../rust/crates/runtime/src/server/router_builder.rs) (integration tests only check registration; this table tracks **system E2E**).

| Group (`router_builder`) | Prefix | System E2E | Notes |
|--------------------------|--------|------------|--------|
| auth | `/auth/` | Yes | `auth_users` in bootstrap / `product_matrix_*` |
| chat | `/chat` | Partial | `/chat/turn` + SSE + `agent_events` in `product_matrix_*`, plus callback HTTP-boundary failures in `e2e_matrix_edge_callback_http_boundary_failures`, duplicate callback handoff coverage in `e2e_matrix_duplicate_tool_result_idempotency` / `e2e_matrix_duplicate_approval_response_idempotency`, mixed-success handoff coverage in `e2e_matrix_chat_turn_partial_batch_failure`, out-of-order callback handoff coverage in `e2e_matrix_chat_turn_out_of_order_tool_results`, concurrent same-session isolation in `e2e_matrix_same_session_concurrent_turns_isolated`, and waiting-turn overlap isolation in `e2e_matrix_same_session_waiting_turn_overlap_isolated`; `POST /chat` + run pause/resume in `e2e_matrix_chat_run_pause_resume_http`; `/chat/stream` smoke in `e2e_matrix_chat_stream_session_info`; no `/chat/ws` E2E |
| sessions | `/sessions` | Yes | CRUD/close/resume/activity + DB |
| admin | `/admin/` | No | Needs admin bootstrap |
| learning | `/api/v1/learning/` | Yes (reads) | Health/signals/stats in `product_matrix_*` |
| agents | `/agents` | Yes | Includes edge register path |
| events | `/events` | Yes | |
| skills | `/skills` | Partial | List/status; not publish/config/resources E2E |
| evaluation | `/evaluation/` | Partial | Reads in `product_matrix_*`; POST write paths not covered in system E2E until implemented; training-data extract/export not in system E2E |
| introspection | `/introspection/` | Yes | |
| branches | `/branches` | No | |
| marketplace | `/marketplace/` | Partial | Quality report / stats / search; not full install/upgrade/rollback/credentials |
| sandbox | `/sandbox` | Yes | |
| workflows | `/workflows` | Partial | `GET /workflows` only |
| platform | `/platform/` | Partial | `GET /platform/snapshot` in `product_matrix_*` |
| runs | `/runs` | Partial | List in `product_matrix_*`; lifecycle in `e2e_matrix_chat_run_pause_resume_http` |
| tasks | `/tasks` | Yes | `e2e_matrix_tasks_lease_and_db_assertions` |
| teams | `/teams` | No | `team_api_integration` + `team_execute_http_integration` (`POST .../execute` with mock executor); Matrix-backed team execute not in system E2E yet |

Additional route families in `router_builder` not named above: **memory** (`/memory/*`), **context** (`/context`), **decisions** (`/decisions`), **models** (`/models`), **jobs** (`/jobs`), **triggers** (`/triggers`), **data-versioning** (`/data-versioning`), **replay** (`/sessions/.../replay`), **reflect** (`/chat/session/.../reflect`), **completions** (`/v1/chat/completions`) — see the P0/P1 table above for E2E status.

## Future work

- **Runs + DB**: when `RunStateStore` is backed by Matrix for `build_server_state`, add SQL assertions alongside `e2e_matrix_chat_run_pause_resume_http`.
- **Evaluation writes**: add a focused test when `validate_gate` / `run_drift_pipeline` / `run_closed_loop` return **200** with stable response shapes.
- **Branches, admin, WS, delegation**: add focused journeys + rows in this matrix.
- **Teams**: optional `system_matrix_http_e2e` journey for `/teams` (including `POST /teams/{name}/execute`) with DB assertions when team store is Matrix-backed in test harness.
- **Real Memoria**: optional second target with a Memoria test double URL instead of the stub forwarder.
