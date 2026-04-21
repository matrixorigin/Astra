# Test coverage matrix (capabilities vs tests)

This document maps **user-visible capabilities** to **where they are tested**, so stub/JSON contract tests can be removed without losing the last line of defense. It extends [`system-e2e-matrix.md`](./system-e2e-matrix.md).

Legend: **E2E** = `rust/crates/runtime/tests/system_matrix_http_e2e/` with `ASTRA_SYSTEM_MATRIX_E2E=1` (real MatrixOne + HTTP + `sqlx` where noted). **Stub** = `build_app(AppState::new(...))` with in-memory/stub services (no DB). **Unit** = `#[cfg(test)]` in crate sources or small integration tests without HTTP.

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
| Replay compare | `journey_full` | `replay_contract` |
| `POST /chat/route` (shape + auth path) | `journey_full` + `e2e_matrix_chat_route_models` | `chat_route_contract` |
| `GET /models` (authenticated list) | `journey_full` + `e2e_matrix_chat_route_models` | — |
| Models admin CRUD + `infra_llm_models` | `journey_extended::run_models_admin_crud_with_db` (`provider: mock`, `grant_astra_admin_role`) | `model_crud_contract` |
| `POST /branches/cost-estimate` (JWT + estimate fields; 401 without auth) | `e2e_matrix_branches_cost_estimate_http` | `branches_contract` (stub; different surface) |
| `GET /admin/tokens` (403 → grant `astra_admin` → 200 array) | `e2e_matrix_admin_tokens_smoke` | — |
| Delegation `GET .../delegations` + `POST .../delegate` validation failure (`400`) | `e2e_matrix_delegate_http_boundaries` | — |
| Reflect + decision-trace (authenticated) | `journey_full` (`GET .../reflect`, `GET .../decision-trace`) | `reflect_contract` (stub) |
| Learning feedback POST | `journey_full` (`POST /api/v1/learning/feedback` with real `event_id` after `chat/turn`) | `reflect_contract` (stub) |
| Skill config CRUD (in-memory stub) | — (future E2E or `astra-runtime` unit tests) | `skill_config_contract` |
| Memory branches API (stub `X-User-Id` auth) | — (future E2E with JWT) | `branches_contract` |
| `POST /streaming/chat` (stub `X-User-Id`) | — (prod uses configured `StreamingService`; `journey_extended` covers `POST /chat/stream` SSE) | `streaming_contract` |
| Route registration (no accidental 404 on major paths) | `runtime/src/server/router_builder.rs` `#[cfg(test)]` (`route_count_regression`, `critical_route_paths_exist`, `all_api_groups_have_routes`) + Matrix `journey_full` | `route_registry_contract` (HTTP smoke) |
| `SharedPool` on `AppState` / auth+session service constructors | Compile-time API + real pool in services tests / Matrix E2E | `shared_pool_contract`, `shared_pool_migration_contract` |
| `/health` includes `persist_ok` / `persist_fail` | `http_contract` + `fixtures/contracts/http_shell_contract.json` | `persist_counter_contract` |
| Global `PERSIST_*` atomics increment | `runtime/src/bridge/side_effects.rs` `#[cfg(test)]` | `persist_counter_contract` |
| Thinking models: `reasoning_content` on every assistant+`tool_calls` after mid-session switch / DB recovery | `runtime/src/turn/edge_ledger.rs` `#[cfg(test)]` (`mid_session_switch_*`, `append_recovered_events` + `strip_stale_reasoning` pipeline) | — (avoid extra integration binary) |

## Large integration binaries (audit — not removed in this pass)

| Binary | Role | E2E overlap | Recommendation |
|--------|------|-------------|----------------|
| `improvement_proofs.rs` | Token/budget/compaction **proofs** vs baselines | None (no HTTP/DB) | **Keep**; move overlapping cases into `astra-runtime` unit tests only if duplicates appear in `src/`. |
| `utterance_regression.rs` | Utterance/tool-selection regression | Partial overlap with `phase8_regression` / cloud routing | **Keep** for NLP surface; dedupe individual cases incrementally if two tests assert the same ranking. |
| `bridge_e2e_comprehensive.rs` | 13 E2E tests covering persistence, multi-turn, cancellation, errors via `bridge-e2e-hooks` mock LLM | `chat_turn_bridge_ledger_inject_e2e`, `edge_cloud_round_trip_e2e` | **Keep**; uses `test_llm_rounds` for deterministic testing without real LLM. |
| Chat turn **pure helpers** (stall, state, persist, routing, cloud/history, …) | `#[cfg(test)]` next to each module under `rust/crates/runtime/src/turn/` | Removed ~33 `chat_turn_*_contract.rs` + matching `fixtures/contracts/chat_turn_*.json` (duplicated JSON snapshots) |
| Run/chat lifecycle (stub `RunLifecycleService` + `/chat/stream` SSE) | — (Matrix journeys exercise `/runs` list and `journey_tasks_runs` for pause/resume) | `chat_lifecycle_contract` |
| Memory prefetch (`prefetch_memories` + mock Memoria HTTP) | `bridge_inprocess.rs` unit tests around `prefetch_memories` | `memory_prefetch_contract` |
| Token / context budget / retrieval JSON tables | `rust/crates/runtime/src/prompts/mod.rs`, `context.rs` `#[cfg(test)]` | `token_retrieval_contract` + `token_retrieval_contract.json` |

## Chat turn / bridge (what remains)

- **Stub integration:** `bridge_e2e_comprehensive.rs` (13 tests) + `edge_cloud_round_trip_e2e.rs` (16 tests) + `chat_turn_bridge_ledger_inject_e2e.rs` — fast CI path without MatrixOne via `bridge-e2e-hooks` feature + `test_llm_rounds` mock mechanism.
- **Logic:** prefer `src/turn/*` unit tests; extend those modules (or Matrix `system_matrix_http_e2e`) instead of new top-level `*_contract.rs` binaries.
- **`/chat/stream` bridge fallback** (lifecycle unconfigured): `runtime/src/server/chat_handlers.rs` → `chat_stream_bridge_fallback_tests` (`#[cfg(test)]`, was `chat_stream_bridge_fallback_contract.rs`).
- **Bridge hook DB side effects** (`build_turn_hook_args` → `run_bridge_hook_side_effects`): `runtime/src/bridge/side_effects.rs` → `inprocess_hook_contract_tests` (`#[cfg(test)]`, was `inprocess_hook_contract.rs`).
- **LLM stream failures (in-process bridge):** `runtime/src/turn/llm_request_dump.rs` — writes `~/.astra/sessions/<id>/llm_error_*.json` and emits `llm_request_dump` via `TurnAuxiliaryEventWriter` from `bridge_inprocess.rs` error paths.

## Services crate

| Test | Gate |
|------|------|
| `astra-services` `multi_agent_integration` | `ASTRA_MULTI_AGENT_IT=1` (PR CI when enabled in [`.github/workflows/test.yml`](../../.github/workflows/test.yml)) |
| `astra-services` `team_persistence_integration` | `ASTRA_MULTI_AGENT_IT=1` + live MatrixOne (`#[ignore]`); see module doc in `rust/crates/services/tests/team_persistence_integration.rs` |

## Team (`/teams/*`, orchestration, persistence)

| Capability | E2E (`system_matrix_http_e2e`) | Stub / integration | Other |
|------------|-------------------------------|----------------------|-------|
| Team CRUD + list/detail + upsert + delete; empty executions list; snapshots create/list/delete; HTTP negatives (401/404/400 validation); HTTP↔DB column fidelity + cross-user isolation | `journey_team_crud_matrix.rs`, `journey_team_snapshots_matrix.rs`, `journey_team_http_negatives_matrix.rs`, `journey_team_data_fidelity_matrix.rs`, `journey_team_isolation_matrix.rs` (`e2e_matrix_team_*` tests); DB: `team_definitions`, `team_snapshots` | `rust/crates/runtime/tests/team_api_integration.rs` (Tower oneshot, `InMemoryTeamStore`, no DB) | — |
| `POST /teams/{name}/execute` (HTTP → `TeamExecutionOrchestrator`, mock `SubRunExecutor`) | — (prod server uses real `ServerSubRunExecutor`; keep execute coverage offline) | `team_execute_http_integration.rs` includes built-in **`review`** + task `review the latest commit` (CLI parity with `/team run review review the latest commit`) happy + failing executor paths | Handler: `team_handlers::execute_team_handler` |
| `TeamExecutionOrchestrator` + `DelegationEngine` (coordination modes, gates, failure paths) | — | `rust/crates/runtime/tests/team_delegation_integration.rs` (`StubSubRunExecutor` + custom `SubRunExecutor` fakes) | `rust/crates/runtime/src/server/team_orchestrator.rs` `#[cfg(test)]` |
| Sub-run uses scripted `MockHost` + `run_agentic_loop_with_host` (non-zero usage vs `StubSubRunExecutor`) | — | — | `team_orchestrator.rs` `mock_host_subrun_*` tests |
| Team definitions + execution history (SQL store) | CRUD + snapshots SQL in team journeys above | — | `team_persistence_integration` (MatrixOne, `#[ignore]`, direct service API) |
| Delegation mailbox with team-shaped agent ids | — | — | `rust/crates/runtime/src/messaging/orchestrator_mailbox_tests.rs` |

## How to run

- Offline slice: `make test-offline` (workspace + bridge hooks; no online `#[ignore]` suites).
- Full validation with MatrixOne: `make test` (`test-offline` then `test-online`) or run `make test-online` alone when you only need ignored suites.
- Advanced: set `ASTRA_SYSTEM_MATRIX_E2E=1` / `ASTRA_MULTI_AGENT_IT=1` manually as in [`system-e2e-matrix.md`](./system-e2e-matrix.md).
