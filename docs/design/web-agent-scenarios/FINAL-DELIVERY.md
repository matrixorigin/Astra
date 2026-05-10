# Astra Web Agent v1 Final Delivery

> Date: 2026-05-08
> Status: v1.0.0 implementation complete; E2E joint verification PASS; known residuals are non-blocking v1.0.1 quality follow-ups.
> Post-E2E hotfix: CSL snapshot loading now uses a MatrixOne-stable `COUNT(*)` probe and assistant final text is included in CSL persistence. Runtime resume does **not** silently rebuild the prompt from `session_transcript_items`; old details are exposed to the LLM through explicit, read-only `session_history_page` / `session_history_search` / `session_history_around` tools so retrieval stays user-scoped, cursor-based, and token-bounded.
> Web UI test hotfix: the resource governor now interprets `max_concurrent_sessions` as sessions with active runs (`agent_runs.status IN ('running','paused','waiting')`), not all durable/resumable historical `agent_sessions`; persisted web chats must not consume execution concurrency after their run completes.
> Web UI usability patch: chats can now be archived into a default-collapsed sidebar section, unarchived, or permanently deleted from the chat menu/sidebar menu. Archived chats are read-only until unarchived. Archive/unarchive persists `agent_sessions.status='archived'/'active'` for sessions with a runtime ID. Permanent delete and clear-archive actions use local inline confirmation near the action and call the runtime `DELETE /sessions/{session_id}` hard-delete path for persisted sessions before removing the Web UI record.

## §1 Design to Implementation Closure

### §1.1 Delivery Chain

```text
14 scenario walkthroughs
  -> 29 tracked gaps
      -> Sprint A/B/C/D design resolutions
          -> 6 implementation phases
              -> 5 cross-phase E2E scenarios
                  -> v1 publishable build
```

The v1 web-agent implementation was driven from the original scenario set rather than from isolated feature tickets:

- **Scenario set**: `S01` through `S14`, covering long dev sessions, huge history recall, flaky network reconnect, approval waits, deep delegation, cross-session memory, personal skills, and small-window edge cases.
- **Gap set**: `G1` through `G29`.
  - `G1`-`G19`: all resolved during Sprint A/B/C.
  - `G20`-`G27`: critical/high end-to-end walkthrough gaps resolved during Sprint D.
  - `G28` and `G29`: intentionally kept as medium follow-ups for v1.0.1.
- **Implementation phases**:
  - Phase 1: durable run state.
  - Phase 2: transcript hydration, cold-start, device lease, Web UI MVP.
  - Phase 3: context manifest, retrieval, budget, confidence.
  - Phase 4: state projection, compaction, delegation, cross-session memory.
  - Phase 5: personal skills.
  - Phase 6: artifact retention, preview templates, downloads.
- **Final E2E set**:
  - E2E-1: `S01` Rust 60-turn refactor chain.
  - E2E-2: `S04` 17 reconnects.
  - E2E-3: `S07` 48h approval across restarts/migration.
  - E2E-4: `S10` five-level delegation and bubble-up.
  - E2E-5: `S14` 8k budget with four-device switching.

### §1.2 Gap Resolution Index

| Gap | Status | Design Resolution | Main Implementation Surface |
| --- | --- | --- | --- |
| G1 | resolved | `§Context Manifest Reason Enum @v0.2` | `context_manifest_reason_types`, reason fallback, manifest write path |
| G2 | resolved | `§Compaction Invariants @v0.2` | `DatabaseStateProjectionStore::run_compaction_assertions` |
| G3 | resolved | `§Retrieval State Machine @v0.2` | retrieval degrade events: `retrieval.*_timeout/empty/stale` |
| G4 | resolved | `§Delegation Contract @v0.2` | `session_delegations`, `delegation_state` projection |
| G5 | resolved | `§Plan Tree Rendering Policy @v0.2` | plan/todo tree budget and Phase 4 UI increment |
| G6 | resolved | `§Cross-Session Scope and User Memory @v0.2` | `session_state_items.scope='user'`, anchor memory load |
| G7 | resolved | `§Approval State and External Notification Adapter @v0.2` | durable waiting state, approval resume chain |
| G8 | resolved | `§Preview Template Registry @v0.2` | `preview_template_registry`, fallback preview warning |
| G9 | resolved | `§Artifact Retention and Access Scope @v0.2` | `session_artifacts`, retention counters, GC sweeper |
| G10 | resolved | `§Small-Window Budget Template @v0.2` | `budget_v1_8k`, `budget_for_turn_intent` |
| G11 | resolved | `§Workspace Reachability and Degradation Semantics @v0.2` | workspace/device lease handoff semantics |
| G12 | resolved | `§Next-Action Confidence State Machine @v0.2` | confidence thresholds and small-model exception |
| G13 | resolved | `§Revision Reconciliation and Device Lease @v0.2` | `session_state_revisions`, `session_device_leases` |
| G14 | resolved | `§Delegation Retry and Bubble-Up Contract @v0.2` | `bubble_up_finding`, retry/superseded state |
| G15 | resolved | `§Run Event Ordering and Ownership @v0.2` | `agent_runs`, `run_counters`, `agent_run_events` |
| G16 | resolved | `§Personal Skill Activation and Evaluation @v0.2` | active skill state item, no-LLM activation path |
| G17 | resolved | `§Content Hash Normalization Contract @v0.2` | SKILL.md and tool output normalization |
| G18 | resolved | `§Delegation State Budget @v0.2` | fan-out and blocker budget allocation |
| G19 | resolved | `§Web Event Watermark Atomicity @v0.2` | IndexedDB transaction + BroadcastChannel protocol |
| G20 | resolved | `§Delegation Tree Artifact ACL @v0.3` | `same_root_tree`, `delegation_direct`, grants table |
| G21 | resolved | `§Delegation State Budget @v0.3` | corrected large fan-out formula |
| G22 | resolved | `§Retry Scope Selection and Propagation @v0.3` | `retry_scope` selection and payload propagation |
| G23 | resolved | `§Tool Output Batch Insert Contract @v0.3` | 500-row batch contract and 1000-row benchmarks |
| G24 | resolved | `§Cold-Start Hydration @v0.3` | `/sessions/{id}/state`, active run replay contract |
| G25 | resolved | `§Device Lease End Event Parity @v0.3` | revoke/auto-expire symmetric SSE events |
| G26 | resolved | `§Manifest Reason Enumeration @v0.3` | extended reason enum and `turn_intent` |
| G27 | resolved | `§Tool Baseline, Raw Ref, and Runner Registration @v0.3` | tool runner registry, raw ref registry, FTS weights |
| G28 | open | deferred | add `cancel` mutation enum in v1.0.1 |
| G29 | open | deferred | formalize `checkpoint_v1.extra` recommendation in v1.0.1 |

### §1.3 Verification Report Index

Design regression reports:

- `REGRESSION-SPRINT-A.md`
- `REGRESSION-SPRINT-B.md`
- `REGRESSION-SPRINT-C.md`
- `REGRESSION-SPRINT-D.md`

Implementation verification reports:

- `IMPL-VERIFY-PHASE-1.md`: `PASS_WITH_WARNINGS`; residuals carried into later phases.
- `IMPL-VERIFY-PHASE-2.md`: `PASS_WITH_WARNINGS`; hard SSE reconnect blocker fixed before Phase 3.
- `IMPL-VERIFY-PHASE-3.md`: `PASS_WITH_WARNINGS`; four critical deviations fixed in Phase 3B.
- `IMPL-VERIFY-PHASE-3B.md`: `PASS`.
- `IMPL-VERIFY-PHASE-4.md`: `PASS_WITH_WARNINGS`; FP3 production wiring fixed in Phase 5.
- `IMPL-VERIFY-PHASE-5.md`: `PASS`.
- `IMPL-VERIFY-PHASE-6.md`: `PASS`.
- `IMPL-VERIFY-E2E-JOINT.md`: `PASS`; 5/5 E2E, 5/5 PERF, 3/3 Phase 6 watchlist closed.

## §2 Code Delivery Inventory

### §2.1 Schema Surface

The web-agent v1 implementation owns or extends **29 schema surfaces**:

| Area | Tables / Extensions |
| --- | --- |
| Session base and audit | `agent_sessions` extended, `agent_events`, `agent_event_edges` |
| Durable runs | `agent_runs`, `run_counters`, `agent_run_events` |
| Tool output durability | `session_tool_output_batches`, `session_tool_outputs` |
| Transcript and device state | `session_transcript_items`, `session_state_revisions`, `session_device_leases`, `session_device_lease_events` |
| Context manifest | `context_manifest_reason_types`, `context_manifests`, `context_manifest_items` |
| Preview/raw-ref registry | `preview_template_registry`, `tool_runner_registry`, `raw_ref_scheme_registry` |
| State projection | `session_state_items`, `session_state_item_events`, `session_delegations`, `session_todos`, `session_history_chunks` |
| Artifacts and ACL | `session_artifacts` extended, `session_artifacts_grants` |
| Personal skills | `user_skill_sources`, `user_skill_versions`, `user_skill_evaluations`, `skill_installations` extended |

The implementation also seeds:

- 25+ context manifest reason values including `progressive_loading`, `intent_driven_preview_expand`, and user-memory reasons.
- 18+ preview templates, currently 19 baselines: `pg_dump`, `fetch_url`, `parse_pdf`, `SKILL.md`, `cargo`, `rustc`, `clippy`, `sql_compat_scan`, `pg_schema_structurize`, `slow_query_analyzer`, `curl`, `git_log`, `docker_logs`, `kubectl`, `python_stdout`, `npm_build`, `csv_head`, `json_preview`, `markdown_preview`.
- Raw-ref schemes including `artifact://`, `s3://`, `conversation_log://`, `tool_output://`, `chunk://`, and `state_item://`.

### §2.2 Rust Modules

Services:

- `rust/crates/services/src/context_manifest.rs`
- `rust/crates/services/src/state_projection.rs`
- `rust/crates/services/src/personal_skills.rs`
- `rust/crates/services/src/artifact_policy.rs`
- `rust/crates/services/src/runs.rs` extended for database-backed durable runs and tool-output batches.
- `rust/crates/services/src/storage.rs` extended for v1 schema creation/migration and registry seed data.

Runtime/server:

- `rust/crates/runtime/src/server/device_lease_sweeper.rs`
- `rust/crates/runtime/src/server/artifact_retention_sweeper.rs`
- `rust/crates/runtime/src/server/user_skill_handlers.rs`
- `rust/crates/runtime/src/server/session_handlers.rs` extended for state, transcript, devices, artifacts, and anchor memory.
- `rust/crates/runtime/src/server/run_handlers.rs` extended for durable run stream/input/cancel.
- `rust/crates/runtime/src/server/run_lifecycle.rs` restores web-agent history from CSL and persists completed assistant text back into CSL. Transcript display rows remain an audit/UI projection, not an automatic prompt source.
- `rust/crates/runtime/src/server/server_tool_executor.rs` exposes read-only session history tools so the LLM can page, search, and expand old transcript details on demand without loading the full session.
- `rust/crates/runtime/src/server/delegation_engine.rs` wired to state projection and bubble-up.
- `rust/crates/runtime/src/server/state_builder.rs` starts device lease and artifact retention sweepers.

Turn/runtime loop:

- `rust/crates/runtime/src/turn/agentic_loop_execution_phase.rs` writes per-LLM-call context manifests and applies turn-intent budget flex.
- `rust/crates/runtime/src/turn/agentic_loop_host.rs`, `bridge_inprocess.rs`, `loop_dispatcher.rs`, and `llm_exchange_capture.rs` were extended for runtime integration and manifest/debug capture.

### §2.3 HTTP Endpoints

Core run endpoints:

| Method | Path |
| --- | --- |
| `GET` / `DELETE` | `/chat/runs/{run_id}` |
| `GET` | `/chat/runs/{run_id}/stream?last_index=N` |
| `POST` | `/chat/runs/{run_id}/pause` |
| `POST` | `/chat/runs/{run_id}/resume` |
| `POST` | `/chat/runs/{run_id}/cancel` |
| `POST` | `/chat/runs/{run_id}/input` |
| `POST` | `/chat/runs/{run_id}/delegate` |
| `GET` | `/chat/runs/{run_id}/delegations` |
| `POST` | `/chat/runs/{run_id}/delegations/pause` |
| `POST` | `/chat/runs/{run_id}/delegations/resume` |

Session and hydration endpoints:

| Method | Path |
| --- | --- |
| `GET` / `POST` | `/sessions` |
| `GET` / `PUT` / `DELETE` | `/sessions/{session_id}` |
| `GET` | `/sessions/{session_id}/state` |
| `GET` | `/sessions/{session_id}/transcript` |
| `GET` | `/sessions/{session_id}/devices` |
| `POST` | `/sessions/{session_id}/device/revoke` |
| `POST` | `/sessions/{session_id}/device/trust` |
| `GET` | `/sessions/{session_id}/device/events` |
| `POST` | `/sessions/{session_id}/close` |
| `POST` | `/sessions/{session_id}/resume` |
| `POST` | `/sessions/{session_id}/cancel` |
| `GET` | `/sessions/{session_id}/activity` |

Artifact endpoints:

| Method | Path |
| --- | --- |
| `GET` | `/sessions/{session_id}/artifacts` |
| `GET` | `/sessions/{session_id}/artifacts/latest/{artifact_kind}` |
| `GET` | `/sessions/{session_id}/artifacts/{artifact_id}` |
| `GET` | `/sessions/{session_id}/artifacts/{artifact_id}/download` |

Personal skill endpoints:

| Method | Path |
| --- | --- |
| `GET` / `POST` | `/skills/user` |
| `GET` / `POST` | `/skills/user/{skill_name}/versions` |
| `POST` | `/skills/user/{skill_name}/evaluations` |
| `POST` | `/skills/user/{skill_name}/activate` |
| `POST` | `/skills/user/{skill_name}/install` |

### §2.4 Frontend Delivery

Web UI MVP items:

1. Login page backed by existing auth sessions.
2. Session list sorted by `updated_at` with pagination.
3. Session chat view: create/open session, keep the composer anchored to the chat viewport while only the transcript pane scrolls, optimistically render the user message, send the turn's `context.thinking` hint to the runtime, forward provider SSE `text_delta` / `reasoning_delta` / `thinking_delta` while they arrive, show a Thinking placeholder while streaming, normalize provider reasoning plus `<think>` / `<thinking>` into a bounded collapsible Thinking timeline with small-region scrolling and per-block "Show more", hide the placeholder after completion if no reasoning was exposed, and return focus to the composer after completion.
4. IndexedDB session cache with atomic run-event and watermark writes.
5. Stop button wired to run/session cancellation endpoint.
6. Device revoke UI in settings/device list.
7. Archived chats section plus read-only archived sessions and inline-confirmed permanent delete/clear-archive actions for chat lifecycle cleanup.
8. Composer Skills picker: paginated/searchable skill selection for large skill catalogs; selected skills are sent as `allow_skills` and mirrored into `context.edge_profile.active_skills` for the runtime turn. The picker and runtime resolver read only database skills visible to the current user (`created_by = current_user OR is_public = 1`); CLI-local filesystem skills remain CLI-only until imported. Runtime does not enumerate the full skill catalog unless `allow_skills` is non-empty.

Incremental v1 UI additions:

- Context token usage indicator and manifest summary entry point.
- `ask_user` candidate UI for confidence ambiguity.
- Plan/Todos tab using `plan-progress`.
- Delegation children panel.
- Personal Skills settings tab.
- Tool-call artifact link and session artifact listing.

Relevant frontend files include:

- `web/hooks/use-chat-stream.ts`
- `web/hooks/use-run-stream.ts`
- `web/lib/session-cache/indexeddb.ts`
- `web/lib/api/session-client.ts`
- `web/lib/api/platform-sessions.ts`
- `web/app/(dashboard)/sessions/page.tsx`
- `web/app/(dashboard)/sessions/[sessionId]/page.tsx`
- `web/components/workspace/tool-timeline.tsx`
- `web/components/workspace/plan-progress.tsx`
- `web/components/workspace/workspace-shell.tsx`
- `web/components/settings/runtime-settings-panel.tsx`

### §2.5 Test Inventory

Implementation and scenario tests:

| File | Test Count |
| --- | ---: |
| `rust/crates/services/tests/schema_assertions.rs` | 6 |
| `rust/crates/runtime/tests/phase1_run_durability.rs` | 11 |
| `rust/crates/runtime/tests/phase2_web_hydration.rs` | 8 |
| `rust/crates/runtime/tests/phase3_context_manifest.rs` | 18 |
| `rust/crates/runtime/tests/phase4_state_projection.rs` | 18 |
| `rust/crates/runtime/tests/phase5_personal_skill.rs` | 7 |
| `rust/crates/runtime/tests/phase6_artifact_preview.rs` | 10 |
| `rust/crates/runtime/tests/e2e_joint.rs` | 5 |
| `rust/crates/runtime/tests/perf_benchmarks.rs` | 5 |

Total tracked Rust verification tests in the v1 implementation path: **88**.

Ignored MatrixOne-backed tests must be run explicitly with:

```bash
ASTRA_TEST_DB_IT=1 ASTRA_AUTO_CREATE_DATABASE=1 MATRIXONE_HOST=127.0.0.1 cargo test -p astra-runtime --test e2e_joint -- --ignored --test-threads=1
ASTRA_TEST_DB_IT=1 ASTRA_AUTO_CREATE_DATABASE=1 MATRIXONE_HOST=127.0.0.1 cargo test -p astra-runtime --test perf_benchmarks -- --ignored --test-threads=1
```

## §3 Known Issues for v1.0.1

These are non-blocking for v1.0.0 because final E2E verification classified them as quality improvements, not missing product behavior.

| ID | Item | Current Status | v1.0.1 Action |
| --- | --- | --- | --- |
| FP-A | E2E-2 connection reset realism | current test drops `reqwest::Response` after HTTP response | upgrade to `hyper::client::conn::handshake` or `axum-test` true connection reset |
| FP-B | PERF-1 P99 sample size | current test uses 40 samples and checks max | increase to at least 256 samples and compute real P99 |
| FP-C | E2E-3 migration realism | current test uses synthetic `CREATE TABLE` migration | add a real core schema version bump migration path |
| WL-2 unit test | GC backlog overflow | production path exists and E2E verifies indirectly | add focused test for `backlog_overflow_warning=true` and `agent_events` write |
| WL-3 FTS recall | preview FTS weights | seed values are present and non-empty | add FTS ranking/recall comparison showing weights affect result order |
| G28 | `session_state_item_events.mutation` missing `cancel` | medium design gap intentionally deferred | add enum value, schema assertion, and contract test |
| G29 | `checkpoint_v1.extra` recommended structure | medium design gap intentionally deferred | document shape and add resume/checkpoint contract test |

Recommended issue titles:

- `v1.0.1: E2E-2 true TCP connection reset harness`
- `v1.0.1: PERF-1 256-sample P99 hot-path benchmark`
- `v1.0.1: E2E-3 real schema version bump migration`
- `v1.0.1: artifact retention backlog overflow unit test`
- `v1.0.1: preview_template FTS weight recall test`
- `v1.0.1: implement G28 cancel mutation`
- `v1.0.1: implement G29 checkpoint_v1.extra contract`

## §4 Operations Manual

### §4.1 Deployment Checklist

Before production deployment:

1. Confirm MatrixOne version and MySQL protocol compatibility used by SQLx.
2. Confirm database user has permissions to create/alter tables and indexes during migration.
3. Run schema bootstrap:

   ```bash
   ASTRA_AUTO_CREATE_DATABASE=1 MATRIXONE_HOST=<host> make test-online
   ```

4. Confirm environment variables:

   ```bash
   MATRIXONE_HOST=<host>
   MATRIXONE_PORT=<port>
   MATRIXONE_USER=<user>
   MATRIXONE_PASSWORD=<password>
   MATRIXONE_DATABASE=<database>
   ASTRA_TEST_DB_IT=1        # CI/verification only
   ASTRA_AUTO_CREATE_DATABASE=1
   ```

5. Verify seed tables:

   ```sql
   SELECT COUNT(*) FROM context_manifest_reason_types;
   SELECT COUNT(*) FROM preview_template_registry WHERE status = 'active';
   SELECT COUNT(*) FROM raw_ref_scheme_registry WHERE is_active = 1;
   ```

6. Verify background sweepers start from server state:

   - Device lease expiry sweeper: every 5 minutes.
   - Artifact retention sweeper: every 1 hour.

7. Confirm web deployment config:

   - API base URL points to the cloud Astra API server.
   - Browser storage uses the IndexedDB watermark protocol.
   - SSE reconnect uses `last_index` from durable local watermark.

### §4.2 CI Pipeline

The normal offline pipeline should run:

```bash
make test-offline
make check
```

The MatrixOne-backed online pipeline must explicitly run ignored suites:

```bash
ASTRA_TEST_DB_IT=1 ASTRA_AUTO_CREATE_DATABASE=1 make test-online
ASTRA_TEST_DB_IT=1 ASTRA_AUTO_CREATE_DATABASE=1 cargo test -p astra-runtime --test e2e_joint -- --ignored --test-threads=1
ASTRA_TEST_DB_IT=1 ASTRA_AUTO_CREATE_DATABASE=1 cargo test -p astra-runtime --test perf_benchmarks -- --ignored --test-threads=1
```

Without `ASTRA_TEST_DB_IT=1`, the ignored L1/L2/L3/E2E tests are not executed and should not be treated as release evidence.

### §4.3 Production Monitoring

Minimum v1 production metrics/events:

| Signal | Expected Meaning |
| --- | --- |
| `context_manifests` write rate | Should be approximately one row per LLM call/turn; sudden drops indicate context assembler bypass. |
| `context_manifest_items.dropped_count` trend | Detects chronic budget pressure or overly broad retrieval. |
| `retrieval.structured_timeout` / `retrieval.fts_empty` / `retrieval.vector_stale` | Retrieval degradation rate; high frequency means context recall quality is regressing. |
| `device_lease_expired` | Passive device expiry volume and stale-browser behavior. |
| `device_revoked` | Explicit revocation audit signal. |
| `preview_template_missing` | Unregistered tool warning; should trend toward zero after tool registry coverage. |
| `artifact_retention_backlog_overflow` | Artifact GC scan limit reached; requires operator attention or shorter sweep interval. |
| `agent_runs.status` distribution | Running/waiting/completed/failed/superseded health. |
| `agent_run_events.event_idx` gaps | Must remain zero gaps per run. |
| `session_artifacts.referenced_by_*_count` | Retention correctness and cold-storage migration eligibility. |

Suggested alert rules:

- Alert if `preview_template_missing` is non-zero for a new release.
- Alert if `artifact_retention_backlog_overflow` appears more than once per hour.
- Alert if `context_manifests` write rate falls below chat turn rate.
- Alert if `retrieval.*_timeout` exceeds normal baseline for 15 minutes.
- Alert if any run has `MAX(event_idx) + 1 != COUNT(DISTINCT event_idx)`.

## §5 v1.0.0 Release Tag Checklist

Before tagging:

1. Merge all Phase PRs and the E2E joint verification changes into main.
2. Ensure git history is organized by phase or by small gap groups.
3. Update `CHANGELOG.md` with:
   - Web agent session durability.
   - Web hydration and device lease support.
   - Context manifest and retrieval contracts.
   - State projection/compaction/delegation.
   - Personal skills.
   - Artifacts, preview templates, and retention.
   - E2E joint verification status.
4. Update `README.md` with:
   - Cloud API server deployment instructions.
   - MatrixOne configuration.
   - Web UI startup instructions.
   - Online verification test commands.
5. Build and push Docker image:

   ```bash
   docker build -t <registry>/astra-api:v1.0.0 .
   docker push <registry>/astra-api:v1.0.0
   ```

6. Run final checks:

   ```bash
   make test
   make check
   ```

7. Tag and push after sanity check:

   ```bash
   git tag v1.0.0
   git push origin v1.0.0
   ```

## §6 Roadmap to v1.0.1

v1.0.1 should be a small hardening release, not a scope expansion release.

Priority order:

1. Close residual test realism items:
   - FP-A true connection reset.
   - FP-B true P99 sampling.
   - FP-C real schema version bump migration.
2. Add focused watchlist tests:
   - WL-2 backlog overflow unit test.
   - WL-3 FTS recall/weighting comparison.
3. Implement medium design gaps:
   - G28 `cancel` mutation.
   - G29 `checkpoint_v1.extra` structure.
4. Pick deferred UI features only after the hardening set is green:
   - Context side panel.
   - Plan/Todo tree visualization beyond the MVP tab.
   - Artifacts gallery with richer browsing.
   - Skill editor and version diff UI.
   - Delegation tree visualization and bubble-up UI.

The v1.0.1 exit condition should be: residual 5/5 closed, G28/G29 closed, no new fatal false-positive in verification.
