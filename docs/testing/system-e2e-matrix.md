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
| `MATRIXONE_DATABASE` | DB | Default `astra_runtime` |
| `JWT_SECRET_KEY` | Auth tokens | Default dev string if unset (not for production) |
| `SECRET_KEY` | App crypto | Default dev string if unset |
| `REDIS_HOST` / `REDIS_PORT` | Cache | Defaults `localhost` / `6379` |
| `EMBEDDING_*` | Embeddings config | `EMBEDDING_DIM` may be required for unknown models (see `AppSettings`) |
| `CHAT_TURN_BRIDGE_SECRET` | Bridge | Default dev string; align with deployment if using real bridge |
| `ASTRA_SYSTEM_MATRIX_E2E_TEST_THREADS` | Makefile only | Set to `1` to run `system_matrix_http_e2e` with `--test-threads=1` (serial) |

Evaluation **read** routes in the full journey use `x-user-id` without bearer (see `journey_full`). Other authenticated calls use the JWT from `bootstrap`.

## Test binaries (ignored by default)

Eight tests total — overlap with the full journey is avoided (e.g. no separate “basic session” test that repeats the same list/get/close/resume steps).

| Test name | File / module | Scope |
|-----------|---------------|-------|
| `product_matrix_api_journey_hits_multiple_tables` | `journey_full.rs` | Full journey: sessions (list/get/put, close/resume, activity, **platform snapshot**), agents, events, context, decisions, memory proxy, edge, jobs, sandbox, triggers, skills, introspection, learning (signals/stats + **POST /api/v1/learning/feedback** after `chat/turn`), evaluation reads, marketplace probe, `chat/turn` SSE + `agent_events`, audit/replay, logout |
| `e2e_matrix_tasks_lease_and_db_assertions` | `journey_tasks_runs.rs` | `POST /tasks`, `GET /tasks`, `GET /tasks/{id}`, `GET .../progress`, `agent_tasks`; edge register; lease claim / `GET` lease / renew / release; `task_leases`; `PUT /tasks/{id}/status` |
| `e2e_matrix_chat_run_pause_resume_http` | `journey_tasks_runs.rs` | `POST /chat` (background run), `POST .../pause`, `GET /chat/runs/{id}`, `POST .../resume` |
| `e2e_matrix_session_cancel_delete` | `journey_extended.rs` | `POST /sessions/{id}/cancel` + `agent_sessions.status`, `DELETE /sessions/{id}` |
| `e2e_matrix_chat_stream_session_info` | `journey_extended.rs` | `POST /chat/stream` SSE → `session_info` + `run_id` |
| `e2e_matrix_auth_session_negative_paths` | `journey_extended.rs` | `GET /sessions` without auth (401); duplicate `POST /auth/register`; bad `POST /auth/login`; successful login |
| `e2e_matrix_memory_proxy_user_isolation` | `journey_extended.rs` | Unauthenticated `POST /memory/store` (401); spoofed `user_id`/`session_id` in body → forwarder receives JWT `user_id` for both fields |
| `e2e_matrix_models_admin_crud` | `journey_extended.rs` | SQL `astra_admin` role grant; `POST/PUT/DELETE /models` with `provider: mock` + `infra_llm_models` row checks |

Shared helpers: `tests/system_matrix_http_e2e/harness.rs` (`bootstrap`, `grant_astra_admin_role`, HTTP helpers, `cleanup_*`, row getters, SSE helpers, `wait_for_agent_event_types` — polls `agent_events` after `chat/turn` instead of a fixed sleep).

## Database isolation

- **Shared database**: All tests use the same MatrixOne database from `AppSettings` (typically `astra_runtime`). There is **no separate schema per test**.
- **Row isolation**: Each `bootstrap()` registers a **new user** (`prod_matrix_{uuid}`), obtains a new `user_id`, creates a **new** `session_id`, and uses an `edge_agent_id` / `suffix` unique to that run. API state and SQL assertions are scoped by those IDs.
- **Parallel runs**: Tests are safe to run in parallel by default (`cargo` / `make test-integration` without `--test-threads=1`). The full journey uses a **suffix-scoped marketplace skill name** (`e2e_matrix_mkt_{suffix}`) so concurrent runs do not fight over the same global marketplace stats key.
- **Opt-in serial**: If you hit flakiness (shared Redis keys, connection limits, etc.), run with `ASTRA_SYSTEM_MATRIX_E2E_TEST_THREADS=1` (see `Makefile` `test-ignored-integration`) to force `--test-threads=1` for `system_matrix_http_e2e` only.

## API groups vs coverage (P0 / P1)

Legend: **DB** = SQL assertion on MatrixOne; **HTTP** = response-only; **—** = not covered by system E2E yet.

| Group | P | Representative routes | Persistence check | Test(s) |
|-------|---|----------------------|-------------------|---------|
| Meta | P0 | `GET /health`, `GET /` | — | `product_matrix_*` |
| Auth | P0 | `/auth/register`, `/login`, `/refresh`, `/me`, `/logout` | `auth_users` | Every test uses `bootstrap` (register/login); `product_matrix_*` also hits `/auth/refresh` and `/logout` |
| Sessions | P0 | `/sessions`, `.../close`, `.../resume`, `.../cancel`, `DELETE ...`, `.../activity` | `agent_sessions` | `product_matrix_*` + `e2e_matrix_session_cancel_delete` |
| Session audit | P0 | `/sessions/{id}/audit/*`, `/audit/*` | mostly HTTP | `product_matrix_*` |
| Agents | P0 | `/agents` CRUD | `agent_agents` | `product_matrix_*` |
| Models | P1 | `GET /models`, admin `POST/PUT/DELETE /models` | `infra_llm_models` | `product_matrix_*` (list); `e2e_matrix_models_admin_crud` (admin CRUD + DB) |
| Events | P0 | `/events`, causal chain, session events | `agent_events` | `product_matrix_*` |
| Context | P0 | `/context` | `ctx_snapshots` | `product_matrix_*` |
| Decisions | P0 | `/decisions`, audit | `ctx_decision_audits` | `product_matrix_*` |
| Memory proxy | P1 | `/memory/*` | Memoria stub calls | `product_matrix_*` |
| Edge §5.5 | P0 | `/agents/edge`, `/tools/result`, `/approval/respond` | `edge_agent_registry` | `product_matrix_*`, tasks lease (edge register) |
| Jobs | P1 | `/jobs`, `/jobs/webhook` | service persistence | `product_matrix_*` |
| Sandbox | P1 | `/sandbox` | `infra_sandbox_metadata` | `product_matrix_*` |
| Triggers | P1 | `/triggers`, fire, delete | `wf_triggers` | `product_matrix_*` |
| Skills / introspection | P1 | `/skills`, `/introspection/*` | mixed | `product_matrix_*` |
| Learning | P1 | `/api/v1/learning/*` | — | `product_matrix_*` |
| Evaluation | P1 | `/evaluation/*` reads | — | `product_matrix_*` |
| Evaluation (writes) | P1 | `POST` gate/validate, drift/run, loop | — | — (no system E2E; add when implementations return success) |
| Marketplace | P1 | quality report, stats, search | marketplace stats tables | `product_matrix_*` |
| Chat turn (SSE) | P0 | `POST /chat/turn` + bridge secret | `agent_events` | `product_matrix_*` |
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

## CI

- **PR** (`.github/workflows/test.yml`): MatrixOne + Redis service containers; `make test` plus ignored **`system_matrix_http_e2e`** and **`multi_agent_integration`** with env vars set in the workflow. See also [`coverage-matrix.md`](./coverage-matrix.md).
- **Manual / nightly**: `.github/workflows/e2e-matrix-nightly.yml` — `workflow_dispatch` with optional **test name filter** (substring) to run a subset (e.g. `e2e_matrix_tasks`) or leave empty for all ignored tests in the binary.

## Router groups alignment

Same prefixes as [`router_builder` `all_api_groups_have_routes`](../../rust/crates/runtime/src/server/router_builder.rs) (integration tests only check registration; this table tracks **system E2E**).

| Group (`router_builder`) | Prefix | System E2E | Notes |
|--------------------------|--------|------------|--------|
| auth | `/auth/` | Yes | `auth_users` in bootstrap / `product_matrix_*` |
| chat | `/chat` | Partial | `/chat/turn` + SSE + `agent_events` in `product_matrix_*`; `POST /chat` + run pause/resume in `e2e_matrix_chat_run_pause_resume_http`; `/chat/stream` smoke in `e2e_matrix_chat_stream_session_info`; no `/chat/ws` E2E |
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

Additional route families in `router_builder` not named above: **memory** (`/memory/*`), **context** (`/context`), **decisions** (`/decisions`), **models** (`/models`), **jobs** (`/jobs`), **triggers** (`/triggers`), **data-versioning** (`/data-versioning`), **replay** (`/sessions/.../replay`), **reflect** (`/chat/session/.../reflect`), **completions** (`/v1/chat/completions`) — see the P0/P1 table above for E2E status.

## Future work

- **Runs + DB**: when `RunStateStore` is backed by Matrix for `build_server_state`, add SQL assertions alongside `e2e_matrix_chat_run_pause_resume_http`.
- **Evaluation writes**: add a focused test when `validate_gate` / `run_drift_pipeline` / `run_closed_loop` return **200** with stable response shapes.
- **Branches, admin, WS, delegation**: add focused journeys + rows in this matrix.
- **Real Memoria**: optional second target with a Memoria test double URL instead of the stub forwarder.
