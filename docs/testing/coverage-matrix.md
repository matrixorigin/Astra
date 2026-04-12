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
| Auth errors (no session token, duplicate user, bad password) | `journey_extended::run_auth_and_session_negative_paths` | `auth_contract` (stub errors) | — |
| Sessions list/get/put, close/resume, activity | `journey_full` + DB `agent_sessions` | `session_contract` | — |
| Session cancel + delete + 404 | `journey_extended` + DB | `session_contract` | — |
| Agents CRUD + DB | `journey_full` + `agent_agents` | `agent_crud_contract` | — |
| Events create/get/list/causal chain | `journey_full` | `event_crud_contract` | — |

## Product API (context, decisions, memory proxy, jobs, sandbox, …)

| Capability | E2E | Removed stub tests |
|------------|-----|-------------------|
| Context + decisions + DB | `journey_full` | `context_contract`, `decisions_contract` |
| Memory proxy isolation (`user_id` / `session_id` overwrite) | `journey_extended::run_memory_proxy_user_isolation` | `memory_contract` |
| Skills + introspection reads | `journey_full` | `skills_contract`, `introspection_contract` |
| Jobs + webhook | `journey_full` | `jobs_contract` |
| Workflows list | `journey_full` | `workflows_contract` |
| Sandbox CRUD + DB | `journey_full` | `sandbox_contract` |
| Triggers + fire + delete + DB | `journey_full` | `triggers_contract` |
| Marketplace probe | `journey_full` | `marketplace_contract` |
| Data versioning lineage | `journey_full` | `data_versioning_contract` |
| Replay compare | `journey_full` | `replay_contract` |
| `POST /chat/route` (shape + auth path) | `journey_full` + unit tests in `runtime/src/server/chat_route.rs` | `chat_route_contract` |
| `GET /models` (authenticated list) | `journey_full` | — |
| Models admin CRUD + `infra_llm_models` | `journey_extended::run_models_admin_crud_with_db` (`provider: mock`, `grant_astra_admin_role`) | `model_crud_contract` |
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
| `chat_turn_bridge_contract.rs` | Many `/chat/turn` + bridge scenarios (stub LLM); upstream SSE rebuild cases grouped in `http_chat_turn_bridge_rebuilds_sanitized_upstream_events` + `internal_rebuild_case!` | `chat_turn_bridge_ledger_inject_e2e`, `journey_full` `chat/turn` + `agent_events` | **Keep** as the single large stub binary; add Matrix E2E when a scenario needs real DB. |
| Chat turn **pure helpers** (stall, state, persist, routing, cloud/history, …) | `#[cfg(test)]` next to each module under `rust/crates/runtime/src/turn/` | Removed ~33 `chat_turn_*_contract.rs` + matching `fixtures/contracts/chat_turn_*.json` (duplicated JSON snapshots) |
| Run/chat lifecycle (stub `RunLifecycleService` + `/chat/stream` SSE) | — (Matrix journeys exercise `/runs` list and `journey_tasks_runs` for pause/resume) | `chat_lifecycle_contract` |
| Memory prefetch (`prefetch_memories` + mock Memoria HTTP) | `bridge_inprocess.rs` unit tests around `prefetch_memories` | `memory_prefetch_contract` |
| Token / context budget / retrieval JSON tables | `rust/crates/runtime/src/prompts/mod.rs`, `context.rs` `#[cfg(test)]` | `token_retrieval_contract` + `token_retrieval_contract.json` |

## Chat turn / bridge (what remains)

- **Stub integration:** `chat_turn_bridge_contract.rs` + `fixtures/contracts/chat_turn_bridge_contract.json` only — fast CI path without MatrixOne. Shared `sse_ok` / `ingest_bridge_capture_from_request` helpers; eight former `http_chat_turn_bridge_rebuilds_*` cases run inside `http_chat_turn_bridge_rebuilds_sanitized_upstream_events` via `internal_rebuild_case!`.
- **Logic:** prefer `src/turn/*` unit tests; extend those modules (or Matrix `system_matrix_http_e2e`) instead of new top-level `*_contract.rs` binaries.
- **`/chat/stream` bridge fallback** (lifecycle unconfigured): `runtime/src/server/chat_handlers.rs` → `chat_stream_bridge_fallback_tests` (`#[cfg(test)]`, was `chat_stream_bridge_fallback_contract.rs`).
- **Bridge hook DB side effects** (`build_turn_hook_args` → `run_bridge_hook_side_effects`): `runtime/src/bridge/side_effects.rs` → `inprocess_hook_contract_tests` (`#[cfg(test)]`, was `inprocess_hook_contract.rs`).
- **LLM stream failures (in-process bridge):** `runtime/src/turn/llm_request_dump.rs` — writes `~/.astra/sessions/<id>/llm_error_*.json` and emits `llm_request_dump` via `TurnAuxiliaryEventWriter` from `bridge_inprocess.rs` error paths.

## Services crate

| Test | Gate |
|------|------|
| `astra-services` `multi_agent_integration` | `ASTRA_MULTI_AGENT_IT=1` (PR CI when enabled in [`.github/workflows/test.yml`](../../.github/workflows/test.yml)) |

## How to run

- Offline slice: `make test-offline` (workspace + bridge hooks; no online `#[ignore]` suites).
- Full validation with MatrixOne: `make test` (`test-offline` then `test-online`) or run `make test-online` alone when you only need ignored suites.
- Advanced: set `ASTRA_SYSTEM_MATRIX_E2E=1` / `ASTRA_MULTI_AGENT_IT=1` manually as in [`system-e2e-matrix.md`](./system-e2e-matrix.md).
