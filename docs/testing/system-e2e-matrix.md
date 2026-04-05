# System E2E matrix (HTTP + MatrixOne)

This document maps **user-visible capabilities** to **HTTP routes**, **persistence (MatrixOne tables or in-process stores)**, and the **integration tests** that assert them. It complements `router_builder.rs` unit tests (route registration only).

## How to run

```bash
cd rust
MO_AGENT_SYSTEM_MATRIX_E2E=1 \
MO_AGENT_BRIDGE_TEST_SECRET=system-matrix-e2e-secret \
cargo test -p astra-runtime --test system_matrix_http_e2e --features bridge-e2e-hooks -- \
  --ignored --nocapture
```

Requires the same environment as `astra-server`: `MATRIXONE_*`, `JWT_SECRET_KEY` / `SECRET_KEY` and related keys via `astra_core::AppSettings::from_env`, etc. Use a local `.env` if you use one for development.

## Environment variables (对照表)

| Variable | Role | Notes |
|----------|------|--------|
| `MO_AGENT_SYSTEM_MATRIX_E2E` | **Gate** | Must be `1` or ignored tests panic in `require_system_e2e_env` |
| `MO_AGENT_BRIDGE_TEST_SECRET` | `/chat/turn` E2E | Injected before parallel runs; must match bridge hook expectations in full journey |
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

Evaluation routes in journeys use `x-user-id` without bearer on some calls (see `journey_full` / `journey_extended`). All other authenticated routes use the JWT from `bootstrap`.

## Test binaries (ignored by default)

| Test name | File / module | Scope |
|-----------|---------------|--------|
| `product_matrix_api_journey_hits_multiple_tables` | `tests/system_matrix_http_e2e/journey_full.rs` | Full journey: sessions, agents, events, context, decisions, memory proxy, edge, jobs, sandbox, triggers, skills, introspection, learning, evaluation reads, marketplace probe, `chat/turn` SSE + `agent_events`, audit/replay, logout |
| `e2e_matrix_basic_auth_session_lifecycle` | `journey_basic.rs` | Bootstrap + session list/get/put, close/resume, `agent_sessions.status`, activity, logout |
| `e2e_matrix_tasks_lease_and_db_assertions` | `journey_tasks_runs.rs` | `POST /tasks`, `agent_tasks`; edge register; lease claim/release; `task_leases`; `PUT /tasks/{id}/status` |
| `e2e_matrix_chat_run_pause_resume_http` | `journey_tasks_runs.rs` | `POST /chat` (background run), `POST .../pause`, `GET /chat/runs/{id}`, `POST .../resume` |
| `e2e_matrix_platform_snapshot` | `journey_extended.rs` | `GET /platform/snapshot` |
| `e2e_matrix_session_cancel_delete` | `journey_extended.rs` | `POST /sessions/{id}/cancel` + `agent_sessions.status`, `DELETE /sessions/{id}` |
| `e2e_matrix_tasks_list_get_lease_renew` | `journey_extended.rs` | `GET /tasks`, `GET /tasks/{id}`, `GET .../progress`, lease `GET` + `renew` |
| `e2e_matrix_evaluation_post_not_implemented` | `journey_extended.rs` | `POST /evaluation/gate/validate`, `POST /evaluation/drift/run`, `POST /evaluation/loop?...` → **501** (until implemented) |
| `e2e_matrix_chat_stream_session_info` | `journey_extended.rs` | `POST /chat/stream` SSE → `session_info` + `run_id` |

Shared helpers: `tests/system_matrix_http_e2e/harness.rs` (`bootstrap`, HTTP helpers, `cleanup_*`, row getters, SSE helpers).

## API groups vs coverage (P0 / P1)

Legend: **DB** = SQL assertion on MatrixOne; **HTTP** = response-only; **—** = not covered by system E2E yet.

| Group | P | Representative routes | Persistence check | Test(s) |
|-------|---|----------------------|-------------------|---------|
| Meta | P0 | `GET /health`, `GET /` | — | `product_matrix_*` |
| Auth | P0 | `/auth/register`, `/login`, `/refresh`, `/me`, `/logout` | `auth_users` | all |
| Sessions | P0 | `/sessions`, `.../close`, `.../resume`, `.../cancel`, `DELETE ...`, `.../activity` | `agent_sessions` | all + `e2e_matrix_session_cancel_delete` |
| Session audit | P0 | `/sessions/{id}/audit/*`, `/audit/*` | mostly HTTP | `product_matrix_*` |
| Agents | P0 | `/agents` CRUD | `agent_agents` | `product_matrix_*` |
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
| Evaluation (writes) | P1 | `POST` gate/validate, drift/run, loop | — | `e2e_matrix_evaluation_post_not_implemented` (expects **501** today) |
| Marketplace | P1 | quality report, stats, search | marketplace stats tables | `product_matrix_*` |
| Chat turn (SSE) | P0 | `POST /chat/turn` + bridge secret | `agent_events` | `product_matrix_*` |
| Chat / runs | P0 | `POST /chat`, `/chat/stream`, `/chat/runs/*` | **In-memory** run store in `build_server_state` (not Matrix table today) | `e2e_matrix_chat_run_pause_resume_http`, `e2e_matrix_chat_stream_session_info` |
| Tasks | P0 | `/tasks`, `GET` list/get/progress, `/tasks/{id}/lease/*`, `.../status` | `agent_tasks`, `task_leases` | `e2e_matrix_tasks_lease_and_db_assertions`, `e2e_matrix_tasks_list_get_lease_renew` |
| Platform | P1 | `GET /platform/snapshot` | — | `e2e_matrix_platform_snapshot` |
| Workflows | P1 | `GET /workflows` | — | `product_matrix_*` |
| Data versioning | P1 | lineage GETs | — | `product_matrix_*` |
| Replay | P1 | `/sessions/{id}/replay/compare` | — | `product_matrix_*` |
| Branches | — | `/branches/*` | — | — |
| Admin | — | `/admin/*` | — | — |
| WebSocket | — | `/chat/ws` | — | — |
| Delegation | — | `/chat/runs/.../delegate` | — | — |

## CI / nightly (optional)

- **PR**: keep default `cargo test --workspace` (ignored tests off).
- **Manual**: `.github/workflows/e2e-matrix-nightly.yml` — `workflow_dispatch` with optional **test name filter** (substring) to run a subset (e.g. `e2e_matrix_basic`) or leave empty for all ignored tests in the binary. Requires MatrixOne + `AppSettings` env (repo secrets/vars) to succeed.

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
| evaluation | `/evaluation/` | Partial | Reads in `product_matrix_*`; POST gate/drift/loop contract in `e2e_matrix_evaluation_post_not_implemented` (**501** until DB writes exist); training-data extract/export not in system E2E |
| introspection | `/introspection/` | Yes | |
| branches | `/branches` | No | |
| marketplace | `/marketplace/` | Partial | Quality report / stats / search; not full install/upgrade/rollback/credentials |
| sandbox | `/sandbox` | Yes | |
| workflows | `/workflows` | Partial | `GET /workflows` only |
| platform | `/platform/` | Partial | `GET /platform/snapshot` in `e2e_matrix_platform_snapshot` |
| runs | `/runs` | Partial | List in `product_matrix_*`; lifecycle in `e2e_matrix_chat_run_pause_resume_http` |
| tasks | `/tasks` | Yes | `e2e_matrix_tasks_lease_and_db_assertions` + `e2e_matrix_tasks_list_get_lease_renew` |

Additional route families in `router_builder` not named above: **memory** (`/memory/*`), **context** (`/context`), **decisions** (`/decisions`), **models** (`/models`), **jobs** (`/jobs`), **triggers** (`/triggers`), **data-versioning** (`/data-versioning`), **replay** (`/sessions/.../replay`), **reflect** (`/chat/session/.../reflect`), **completions** (`/v1/chat/completions`) — see the P0/P1 table above for E2E status.

## Future work

- **Runs + DB**: when `RunStateStore` is backed by Matrix for `build_server_state`, add SQL assertions alongside `e2e_matrix_chat_run_pause_resume_http`.
- **Evaluation writes**: when `validate_gate` / `run_drift_pipeline` / `run_closed_loop` return success, change `e2e_matrix_evaluation_post_not_implemented` to assert **200** + response shape (and keep or rename the test).
- **Branches, admin, WS, delegation**: add focused journeys + rows in this matrix.
- **Real Memoria**: optional second target with a Memoria test double URL instead of the stub forwarder.
