# Test coverage matrix (capabilities vs tests)

This document maps **user-visible capabilities** to **where they are tested**, so stub/JSON contract tests can be removed without losing the last line of defense. It extends [`system-e2e-matrix.md`](./system-e2e-matrix.md).

Legend: **E2E** = `rust/crates/runtime/tests/system_matrix_http_e2e/` with `ASTRA_SYSTEM_MATRIX_E2E=1` (real MatrixOne + HTTP + `sqlx` where noted). **Stub** = `build_app(AppState::new(...))` with in-memory/stub services (no DB). **Unit** = `#[cfg(test)]` in crate sources or small integration tests without HTTP.

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

## Large integration binaries (audit — not removed in this pass)

| Binary | Role | E2E overlap | Recommendation |
|--------|------|-------------|----------------|
| `improvement_proofs.rs` | Token/budget/compaction **proofs** vs baselines | None (no HTTP/DB) | **Keep**; move overlapping cases into `astra-runtime` unit tests only if duplicates appear in `src/`. |
| `utterance_regression.rs` | Utterance/tool-selection regression | Partial overlap with `phase8_regression` / cloud routing | **Keep** for NLP surface; dedupe individual cases incrementally if two tests assert the same ranking. |
| `chat_turn_bridge_contract.rs` | Many `/chat/turn` + bridge scenarios (stub LLM) | `chat_turn_bridge_ledger_inject_e2e`, `journey_full` `chat/turn` + `agent_events` | **Consolidate incrementally**: prefer new journey steps + DB over new stub scenarios; do not delete wholesale without mapping each scenario. |

## Chat turn / bridge (stub JSON contracts)

The `chat_turn_*_contract.rs` family and `fixtures/contracts/chat_turn_*.json` remain for fast CI paths that do not start MatrixOne. Migrate scenario-by-scenario into **E2E** when the same behavior is asserted with **real persistence** (`agent_events`, `ctx_*`, etc.). See the P0/P1 table in [`system-e2e-matrix.md`](./system-e2e-matrix.md).

## Services crate

| Test | Gate |
|------|------|
| `astra-services` `multi_agent_integration` | `ASTRA_MULTI_AGENT_IT=1` (PR CI when enabled in [`.github/workflows/test.yml`](../../.github/workflows/test.yml)) |

## How to run

- Default workspace: `make test` (no live DB for ignored E2E unless env set).
- Full Matrix E2E + multi-agent: `make test-integration` or set `ASTRA_SYSTEM_MATRIX_E2E=1` / `ASTRA_MULTI_AGENT_IT=1` as in [`system-e2e-matrix.md`](./system-e2e-matrix.md).
